// Copyright (c) SimpleStaking and Tezedge Contributors
// SPDX-License-Identifier: MIT

//! Listens for events from the `protocol_runner`.

use bytes::Buf;
use failure::Error;
use riker::actors::*;
use slog::{crit, debug, info, warn, Logger};
use std::convert::TryFrom;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use crypto::hash::{BlockHash, ContextHash, FromBytesError, HashType};
use storage::context::{ContextApi, TezedgeContext, TreeId};
use storage::merkle_storage::EntryHash;
use storage::persistent::{ActionRecorder, PersistentStorage};
use storage::BlockStorage;
use tezos_context::channel::ContextAction;
use tezos_wrapper::service::IpcEvtServer;

use crate::shell_channel::{ShellChannelMsg, ShellChannelRef};
use crate::subscription::subscribe_to_shell_shutdown;

type SharedJoinHandle = Arc<Mutex<Option<JoinHandle<Result<(), Error>>>>>;

/// This actor listens for events generated by the `protocol_runner`.
#[actor(ShellChannelMsg)]
pub struct ContextListener {
    /// Just for subscribing to shell shutdown channel
    shell_channel: ShellChannelRef,

    /// Thread where blocks are applied will run until this is set to `false`
    listener_run: Arc<AtomicBool>,
    /// Context event listener thread
    listener_thread: SharedJoinHandle,
}

/// Reference to [context listener](ContextListener) actor.
pub type ContextListenerRef = ActorRef<ContextListenerMsg>;

impl ContextListener {
    // TODO: if needed, can go to cfg
    const IPC_ACCEPT_TIMEOUT: Duration = Duration::from_secs(3);

    /// Create new actor instance.
    ///
    /// This actor spawns a new thread in which it listens for incoming events from the `protocol_runner`.
    /// Events are received from IPC channel provided by [`event_server`](IpcEvtServer).
    pub fn actor(
        sys: &impl ActorRefFactory,
        shell_channel: ShellChannelRef,
        persistent_storage: &PersistentStorage,
        action_store_backend: Vec<Box<dyn ActionRecorder + Send>>,
        mut event_server: IpcEvtServer,
        log: Logger,
    ) -> Result<ContextListenerRef, CreateError> {
        let listener_run = Arc::new(AtomicBool::new(true));
        let block_applier_thread = {
            let listener_run = listener_run.clone();
            let persistent_storage = persistent_storage.clone();

            thread::spawn(move || -> Result<(), Error> {
                let mut context: Box<dyn ContextApi> = Box::new(TezedgeContext::new(
                    BlockStorage::new(&persistent_storage),
                    persistent_storage.merkle(),
                ));

                let mut action_store_backend = action_store_backend;

                while listener_run.load(Ordering::Acquire) {
                    match listen_protocol_events(
                        &listener_run,
                        &mut event_server,
                        Self::IPC_ACCEPT_TIMEOUT,
                        &mut action_store_backend,
                        &mut context,
                        &log,
                    ) {
                        Ok(()) => info!(log, "Context listener finished"),
                        Err(err) => {
                            if listener_run.load(Ordering::Acquire) {
                                crit!(log, "Error process context event"; "reason" => format!("{:?}", err))
                            }
                        }
                    }
                }

                info!(log, "Context listener thread finished");
                Ok(())
            })
        };

        let myself = sys.actor_of_props::<ContextListener>(
            ContextListener::name(),
            Props::new_args((
                shell_channel,
                listener_run,
                Arc::new(Mutex::new(Some(block_applier_thread))),
            )),
        )?;

        Ok(myself)
    }

    /// The `ContextListener` is intended to serve as a singleton actor so that's why
    /// we won't support multiple names per instance.
    fn name() -> &'static str {
        "context-listener"
    }
}

impl ActorFactoryArgs<(ShellChannelRef, Arc<AtomicBool>, SharedJoinHandle)> for ContextListener {
    fn create_args(
        (shell_channel, listener_run, listener_thread): (
            ShellChannelRef,
            Arc<AtomicBool>,
            SharedJoinHandle,
        ),
    ) -> Self {
        ContextListener {
            shell_channel,
            listener_run,
            listener_thread,
        }
    }
}

impl Actor for ContextListener {
    type Msg = ContextListenerMsg;

    fn pre_start(&mut self, ctx: &Context<Self::Msg>) {
        subscribe_to_shell_shutdown(&self.shell_channel, ctx.myself());
    }

    fn post_stop(&mut self) {
        self.listener_run.store(false, Ordering::Release);

        let _ = self
            .listener_thread
            .lock()
            .unwrap()
            .take()
            .expect("Thread join handle is missing")
            .join()
            .expect("Failed to join context listener thread");
    }

    fn recv(&mut self, ctx: &Context<Self::Msg>, msg: Self::Msg, sender: Sender) {
        self.receive(ctx, msg, sender);
    }
}

impl Receive<ShellChannelMsg> for ContextListener {
    type Msg = ContextListenerMsg;

    fn receive(&mut self, _: &Context<Self::Msg>, msg: ShellChannelMsg, _sender: Sender) {
        if let ShellChannelMsg::ShuttingDown(_) = msg {
            self.listener_run.store(false, Ordering::Release);
        }
    }
}

fn listen_protocol_events(
    apply_block_run: &AtomicBool,
    event_server: &mut IpcEvtServer,
    event_server_accept_timeout: Duration,
    action_store_backend: &mut Vec<Box<dyn ActionRecorder + Send>>,
    context: &mut Box<dyn ContextApi>,
    log: &Logger,
) -> Result<(), Error> {
    info!(
        log,
        "Context listener is waiting for connection from protocol runner"
    );
    let mut rx = event_server.try_accept(event_server_accept_timeout)?;
    info!(
        log,
        "Context listener received connection from protocol runner. Starting to process context events."
    );

    let mut event_count = 0;

    while apply_block_run.load(Ordering::Acquire) {
        match rx.receive() {
            Ok(ContextAction::Shutdown) => {
                // when we receive shutting down, it means just that protocol runner disconnected
                // we dont want to stop context listener here, for example, because we are just restarting protocol runner
                // and we want to wait for a new one to try_accept
                // if we want to shutdown context listener, there is ShellChannelMsg for that
                break;
            }
            Ok(action) => {
                if event_count % 100 == 0 {
                    debug!(
                        log,
                        "Received protocol event";
                        "count" => event_count,
                        "context_hash" => match &context.get_last_commit_hash() {
                            None => "-none-".to_string(),
                            Some(c) => HashType::ContextHash.hash_to_b58check(c)?
                        }
                    );
                }

                event_count += 1;

                for recorder in action_store_backend.iter_mut() {
                    if let Err(error) = recorder.record(&action) {
                        warn!(log, "Failed to store context action"; "action" => format!("{:?}", &action), "reason" => format!("{}", error));
                    }
                }

                perform_context_action(&action, context)?;
                // below logic should be driven by dedicated ContextAction events
                if let ContextAction::Commit { .. } = &action {
                    context.block_applied()?;
                    if event_count > 0 && event_count % 4096 == 0 {
                        context.cycle_started()?;
                    }
                }
            }
            Err(err) => {
                warn!(log, "Failed to receive event from protocol runner"; "reason" => format!("{:?}", err));
                break;
            }
        }
    }

    Ok(())
}

pub fn get_tree_id(action: &ContextAction) -> Option<TreeId> {
    match &action {
        ContextAction::Get { tree_id, .. }
        | ContextAction::Mem { tree_id, .. }
        | ContextAction::DirMem { tree_id, .. }
        | ContextAction::Set { tree_id, .. }
        | ContextAction::Copy { tree_id, .. }
        | ContextAction::Delete { tree_id, .. }
        | ContextAction::RemoveRecursively { tree_id, .. }
        | ContextAction::Commit { tree_id, .. }
        | ContextAction::Fold { tree_id, .. } => Some(*tree_id),
        ContextAction::Checkout { .. } | ContextAction::Shutdown => None,
    }
}

pub fn get_new_tree_hash(action: &ContextAction) -> Option<EntryHash> {
    match &action {
        ContextAction::Set { new_tree_hash, .. }
        | ContextAction::Copy { new_tree_hash, .. }
        | ContextAction::Delete { new_tree_hash, .. }
        | ContextAction::RemoveRecursively { new_tree_hash, .. } => {
            new_tree_hash.clone().map(|hash| {
                let mut buffer: EntryHash = [0; 32];
                hash.reader().read_exact(&mut buffer).unwrap();
                Some(buffer)
            })?
        }
        ContextAction::Get { .. }
        | ContextAction::Mem { .. }
        | ContextAction::DirMem { .. }
        | ContextAction::Commit { .. }
        | ContextAction::Fold { .. }
        | ContextAction::Checkout { .. }
        | ContextAction::Shutdown => None,
    }
}

fn try_from_untyped_option<H>(h: &Option<Vec<u8>>) -> Result<Option<H>, FromBytesError>
where
    H: TryFrom<Vec<u8>, Error = FromBytesError>,
{
    h.as_ref()
        .map(|h| H::try_from(h.clone()))
        .map_or(Ok(None), |r| r.map(Some))
}

pub fn perform_context_action(
    action: &ContextAction,
    context: &mut Box<dyn ContextApi>,
) -> Result<(), Error> {
    if let Some(tree_id) = get_tree_id(&action) {
        context.set_merkle_root(tree_id)?;
    }

    match action {
        ContextAction::Get { key, .. } => {
            context.get_key(key)?;
        }
        ContextAction::Mem { key, .. } => {
            context.mem(key)?;
        }
        ContextAction::DirMem { key, .. } => {
            context.dirmem(key)?;
        }
        ContextAction::Set {
            key,
            value,
            new_tree_id,
            context_hash,
            ..
        } => {
            let context_hash = try_from_untyped_option(context_hash)?;
            context.set(&context_hash, *new_tree_id, key, value)?;
        }
        ContextAction::Copy {
            to_key: key,
            from_key,
            new_tree_id,
            context_hash,
            ..
        } => {
            let context_hash = try_from_untyped_option(context_hash)?;
            context.copy_to_diff(&context_hash, *new_tree_id, from_key, key)?;
        }
        ContextAction::Delete {
            key,
            new_tree_id,
            context_hash,
            ..
        } => {
            let context_hash = try_from_untyped_option(context_hash)?;
            context.delete_to_diff(&context_hash, *new_tree_id, key)?;
        }
        ContextAction::RemoveRecursively {
            key,
            new_tree_id,
            context_hash,
            ..
        } => {
            let context_hash = try_from_untyped_option(context_hash)?;
            context.remove_recursively_to_diff(&context_hash, *new_tree_id, key)?;
        }
        ContextAction::Commit {
            parent_context_hash,
            new_context_hash,
            block_hash: Some(block_hash),
            author,
            message,
            date,
            ..
        } => {
            let parent_context_hash = try_from_untyped_option(parent_context_hash)?;
            let block_hash = BlockHash::try_from(block_hash.clone())?;
            let new_context_hash = ContextHash::try_from(new_context_hash.clone())?;
            let hash = context.commit(
                &block_hash,
                &parent_context_hash,
                author.to_string(),
                message.to_string(),
                *date,
            )?;
            assert_eq!(
                &hash,
                &new_context_hash,
                "Invalid context_hash for block: {}, expected: {}, but was: {}",
                block_hash.to_base58_check(),
                new_context_hash.to_base58_check(),
                hash.to_base58_check(),
            );
        }

        ContextAction::Checkout { context_hash, .. } => {
            context.checkout(&ContextHash::try_from(context_hash.clone())?)?;
        }

        ContextAction::Commit { .. } => (), // Ignored (no block_hash)

        ContextAction::Fold { .. } => (), // Ignored

        ContextAction::Shutdown => (), // Ignored
    };

    if let Some(post_hash) = get_new_tree_hash(&action) {
        assert_eq!(context.get_merkle_root(), post_hash);
    }

    Ok(())
}
