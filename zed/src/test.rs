use crate::{
    assets::Assets,
    channel::ChannelList,
    fs::RealFs,
    http::{HttpClient, Request, Response, ServerResponse},
    language::LanguageRegistry,
    presence::Presence,
    rpc::{self, Client, Credentials, EstablishConnectionError},
    settings::{self, ThemeRegistry},
    time::ReplicaId,
    user::UserStore,
    AppState,
};
use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, Future};
use gpui::{AsyncAppContext, Entity, ModelHandle, MutableAppContext, TestAppContext};
use parking_lot::Mutex;
use postage::{mpsc, prelude::Stream as _};
use smol::channel;
use std::{
    fmt,
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        Arc,
    },
};
use tempdir::TempDir;
use zrpc::{proto, Connection, ConnectionId, Peer, Receipt, TypedEnvelope};

#[cfg(test)]
#[ctor::ctor]
fn init_logger() {
    env_logger::init();
}

#[derive(Clone)]
struct Envelope<T: Clone> {
    message: T,
    sender: ReplicaId,
}

#[cfg(test)]
pub(crate) struct Network<T: Clone, R: rand::Rng> {
    inboxes: std::collections::BTreeMap<ReplicaId, Vec<Envelope<T>>>,
    all_messages: Vec<T>,
    rng: R,
}

#[cfg(test)]
impl<T: Clone, R: rand::Rng> Network<T, R> {
    pub fn new(rng: R) -> Self {
        Network {
            inboxes: Default::default(),
            all_messages: Vec::new(),
            rng,
        }
    }

    pub fn add_peer(&mut self, id: ReplicaId) {
        self.inboxes.insert(id, Vec::new());
    }

    pub fn is_idle(&self) -> bool {
        self.inboxes.values().all(|i| i.is_empty())
    }

    pub fn broadcast(&mut self, sender: ReplicaId, messages: Vec<T>) {
        for (replica, inbox) in self.inboxes.iter_mut() {
            if *replica != sender {
                for message in &messages {
                    let min_index = inbox
                        .iter()
                        .enumerate()
                        .rev()
                        .find_map(|(index, envelope)| {
                            if sender == envelope.sender {
                                Some(index + 1)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);

                    // Insert one or more duplicates of this message *after* the previous
                    // message delivered by this replica.
                    for _ in 0..self.rng.gen_range(1..4) {
                        let insertion_index = self.rng.gen_range(min_index..inbox.len() + 1);
                        inbox.insert(
                            insertion_index,
                            Envelope {
                                message: message.clone(),
                                sender,
                            },
                        );
                    }
                }
            }
        }
        self.all_messages.extend(messages);
    }

    pub fn has_unreceived(&self, receiver: ReplicaId) -> bool {
        !self.inboxes[&receiver].is_empty()
    }

    pub fn receive(&mut self, receiver: ReplicaId) -> Vec<T> {
        let inbox = self.inboxes.get_mut(&receiver).unwrap();
        let count = self.rng.gen_range(0..inbox.len() + 1);
        inbox
            .drain(0..count)
            .map(|envelope| envelope.message)
            .collect()
    }
}

pub fn sample_text(rows: usize, cols: usize) -> String {
    let mut text = String::new();
    for row in 0..rows {
        let c: char = ('a' as u32 + row as u32) as u8 as char;
        let mut line = c.to_string().repeat(cols);
        if row < rows - 1 {
            line.push('\n');
        }
        text += &line;
    }
    text
}

pub fn temp_tree(tree: serde_json::Value) -> TempDir {
    let dir = TempDir::new("").unwrap();
    write_tree(dir.path(), tree);
    dir
}

fn write_tree(path: &Path, tree: serde_json::Value) {
    use serde_json::Value;
    use std::fs;

    if let Value::Object(map) = tree {
        for (name, contents) in map {
            let mut path = PathBuf::from(path);
            path.push(name);
            match contents {
                Value::Object(_) => {
                    fs::create_dir(&path).unwrap();
                    write_tree(&path, contents);
                }
                Value::Null => {
                    fs::create_dir(&path).unwrap();
                }
                Value::String(contents) => {
                    fs::write(&path, contents).unwrap();
                }
                _ => {
                    panic!("JSON object must contain only objects, strings, or null");
                }
            }
        }
    } else {
        panic!("You must pass a JSON object to this helper")
    }
}

pub fn test_app_state(cx: &mut MutableAppContext) -> Arc<AppState> {
    let (settings_tx, settings) = settings::test(cx);
    let languages = Arc::new(LanguageRegistry::new());
    let themes = ThemeRegistry::new(Assets, cx.font_cache().clone());
    let rpc = rpc::Client::new();
    let http = FakeHttpClient::new(|_| async move { Ok(ServerResponse::new(404)) });
    let user_store = cx.add_model(|cx| UserStore::new(rpc.clone(), http, cx));
    Arc::new(AppState {
        settings_tx: Arc::new(Mutex::new(settings_tx)),
        settings,
        themes,
        languages: languages.clone(),
        channel_list: cx.add_model(|cx| ChannelList::new(user_store.clone(), rpc.clone(), cx)),
        rpc,
        user_store,
        fs: Arc::new(RealFs),
    })
}

pub struct Observer<T>(PhantomData<T>);

impl<T: 'static> Entity for Observer<T> {
    type Event = ();
}

impl<T: Entity> Observer<T> {
    pub fn new(
        handle: &ModelHandle<T>,
        cx: &mut gpui::TestAppContext,
    ) -> (ModelHandle<Self>, channel::Receiver<()>) {
        let (notify_tx, notify_rx) = channel::unbounded();
        let observer = cx.add_model(|cx| {
            cx.observe(handle, move |_, _, _| {
                let _ = notify_tx.try_send(());
            })
            .detach();
            Observer(PhantomData)
        });
        (observer, notify_rx)
    }
}

pub struct FakeServer {
    peer: Arc<Peer>,
    incoming: Mutex<Option<mpsc::Receiver<Box<dyn proto::AnyTypedEnvelope>>>>,
    connection_id: Mutex<Option<ConnectionId>>,
    forbid_connections: AtomicBool,
    auth_count: AtomicUsize,
    access_token: AtomicUsize,
    user_id: u64,
}

impl FakeServer {
    pub async fn for_client(
        client_user_id: u64,
        client: &mut Arc<Client>,
        cx: &TestAppContext,
    ) -> Arc<Self> {
        let server = Arc::new(Self {
            peer: Peer::new(),
            incoming: Default::default(),
            connection_id: Default::default(),
            forbid_connections: Default::default(),
            auth_count: Default::default(),
            access_token: Default::default(),
            user_id: client_user_id,
        });

        Arc::get_mut(client)
            .unwrap()
            .override_authenticate({
                let server = server.clone();
                move |cx| {
                    server.auth_count.fetch_add(1, SeqCst);
                    let access_token = server.access_token.load(SeqCst).to_string();
                    cx.spawn(move |_| async move {
                        Ok(Credentials {
                            user_id: client_user_id,
                            access_token,
                        })
                    })
                }
            })
            .override_establish_connection({
                let server = server.clone();
                move |credentials, cx| {
                    let credentials = credentials.clone();
                    cx.spawn({
                        let server = server.clone();
                        move |cx| async move { server.establish_connection(&credentials, &cx).await }
                    })
                }
            });

        client
            .authenticate_and_connect(&cx.to_async())
            .await
            .unwrap();
        server
    }

    pub async fn disconnect(&self) {
        self.peer.disconnect(self.connection_id()).await;
        self.connection_id.lock().take();
        self.incoming.lock().take();
    }

    async fn establish_connection(
        &self,
        credentials: &Credentials,
        cx: &AsyncAppContext,
    ) -> Result<Connection, EstablishConnectionError> {
        assert_eq!(credentials.user_id, self.user_id);

        if self.forbid_connections.load(SeqCst) {
            Err(EstablishConnectionError::Other(anyhow!(
                "server is forbidding connections"
            )))?
        }

        if credentials.access_token != self.access_token.load(SeqCst).to_string() {
            Err(EstablishConnectionError::Unauthorized)?
        }

        let (client_conn, server_conn, _) = Connection::in_memory();
        let (connection_id, io, incoming) = self.peer.add_connection(server_conn).await;
        cx.background().spawn(io).detach();
        *self.incoming.lock() = Some(incoming);
        *self.connection_id.lock() = Some(connection_id);
        Ok(client_conn)
    }

    pub fn auth_count(&self) -> usize {
        self.auth_count.load(SeqCst)
    }

    pub fn roll_access_token(&self) {
        self.access_token.fetch_add(1, SeqCst);
    }

    pub fn forbid_connections(&self) {
        self.forbid_connections.store(true, SeqCst);
    }

    pub fn allow_connections(&self) {
        self.forbid_connections.store(false, SeqCst);
    }

    pub async fn send<T: proto::EnvelopedMessage>(&self, message: T) {
        self.peer.send(self.connection_id(), message).await.unwrap();
    }

    pub async fn receive<M: proto::EnvelopedMessage>(&self) -> Result<TypedEnvelope<M>> {
        let message = self
            .incoming
            .lock()
            .as_mut()
            .expect("not connected")
            .recv()
            .await
            .ok_or_else(|| anyhow!("other half hung up"))?;
        let type_name = message.payload_type_name();
        Ok(*message
            .into_any()
            .downcast::<TypedEnvelope<M>>()
            .unwrap_or_else(|_| {
                panic!(
                    "fake server received unexpected message type: {:?}",
                    type_name
                );
            }))
    }

    pub async fn respond<T: proto::RequestMessage>(
        &self,
        receipt: Receipt<T>,
        response: T::Response,
    ) {
        self.peer.respond(receipt, response).await.unwrap()
    }

    fn connection_id(&self) -> ConnectionId {
        self.connection_id.lock().expect("not connected")
    }
}

pub struct FakeHttpClient {
    handler:
        Box<dyn 'static + Send + Sync + Fn(Request) -> BoxFuture<'static, Result<ServerResponse>>>,
}

impl FakeHttpClient {
    pub fn new<Fut, F>(handler: F) -> Arc<dyn HttpClient>
    where
        Fut: 'static + Send + Future<Output = Result<ServerResponse>>,
        F: 'static + Send + Sync + Fn(Request) -> Fut,
    {
        Arc::new(Self {
            handler: Box::new(move |req| Box::pin(handler(req))),
        })
    }
}

impl fmt::Debug for FakeHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeHttpClient").finish()
    }
}

impl HttpClient for FakeHttpClient {
    fn send<'a>(&'a self, req: Request) -> BoxFuture<'a, Result<Response>> {
        let future = (self.handler)(req);
        Box::pin(async move { future.await.map(Into::into) })
    }
}
