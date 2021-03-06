use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};

use wayland_commons::map::{Object, ObjectMap, ObjectMetadata};
use wayland_commons::MessageGroup;

use super::connection::Connection;
use super::queues::QueueBuffer;
use super::{Dispatcher, EventQueueInner};
use {Implementation, Interface, Proxy};

#[derive(Clone)]
pub(crate) struct ObjectMeta {
    pub(crate) buffer: QueueBuffer,
    pub(crate) alive: Arc<AtomicBool>,
    pub(crate) user_data: Arc<AtomicPtr<()>>,
    pub(crate) dispatcher: Arc<Mutex<Dispatcher>>,
    pub(crate) server_destroyed: bool,
    pub(crate) client_destroyed: bool,
}

impl ObjectMetadata for ObjectMeta {
    fn child(&self) -> ObjectMeta {
        ObjectMeta {
            buffer: self.buffer.clone(),
            alive: Arc::new(AtomicBool::new(true)),
            user_data: Arc::new(AtomicPtr::new(::std::ptr::null_mut())),
            dispatcher: super::default_dispatcher(),
            server_destroyed: false,
            client_destroyed: false,
        }
    }
}

impl ObjectMeta {
    pub(crate) fn new(buffer: QueueBuffer) -> ObjectMeta {
        ObjectMeta {
            buffer,
            alive: Arc::new(AtomicBool::new(true)),
            user_data: Arc::new(AtomicPtr::new(::std::ptr::null_mut())),
            dispatcher: super::default_dispatcher(),
            server_destroyed: false,
            client_destroyed: false,
        }
    }

    fn dead() -> ObjectMeta {
        ObjectMeta {
            buffer: super::queues::create_queue_buffer(),
            alive: Arc::new(AtomicBool::new(false)),
            user_data: Arc::new(AtomicPtr::new(::std::ptr::null_mut())),
            dispatcher: super::default_dispatcher(),
            server_destroyed: true,
            client_destroyed: true,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ProxyInner {
    pub(crate) map: Arc<Mutex<ObjectMap<ObjectMeta>>>,
    pub(crate) connection: Arc<Mutex<Connection>>,
    pub(crate) object: Object<ObjectMeta>,
    pub(crate) id: u32,
}

impl ProxyInner {
    pub(crate) fn from_id(
        id: u32,
        map: Arc<Mutex<ObjectMap<ObjectMeta>>>,
        connection: Arc<Mutex<Connection>>,
    ) -> Option<ProxyInner> {
        let me = map.lock().unwrap().find(id);
        me.map(|obj| ProxyInner {
            map,
            connection,
            id,
            object: obj,
        })
    }

    pub(crate) fn is_interface<I: Interface>(&self) -> bool {
        self.object.is_interface::<I>()
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.object.meta.alive.load(Ordering::Acquire)
    }

    pub fn version(&self) -> u32 {
        self.object.version
    }

    pub(crate) fn id(&self) -> u32 {
        if self.is_alive() {
            self.id
        } else {
            0
        }
    }

    pub fn set_user_data(&self, ptr: *mut ()) {
        self.object.meta.user_data.store(ptr, Ordering::Release)
    }

    pub fn get_user_data(&self) -> *mut () {
        self.object.meta.user_data.load(Ordering::Acquire)
    }

    pub(crate) fn send<I: Interface>(&self, msg: I::Request) {
        // grab the connection lock before anything else
        // this avoids the risk of marking ourselve dead while an other
        // thread is sending a message an accidentaly sending that message
        // after ours if ours is a destructor
        let mut conn_lock = self.connection.lock().unwrap();
        if !self.is_alive() {
            return;
        }
        let destructor = msg.is_destructor();
        let msg = msg.into_raw(self.id);
        if ::std::env::var_os("WAYLAND_DEBUG").is_some() {
            println!(
                " -> {}@{}: {} {:?}",
                I::NAME,
                self.id,
                self.object.requests[msg.opcode as usize].name,
                msg.args
            );
        }
        // TODO: figure our if this can fail and still be recoverable ?
        let _ = conn_lock.write_message(&msg).expect("Sending a message failed.");
        if destructor {
            self.object.meta.alive.store(false, Ordering::Release);
            {
                // cleanup the map as appropriate
                let mut map = conn_lock.map.lock().unwrap();
                let server_destroyed = map.with(self.id, |obj| {
                    obj.meta.client_destroyed = true;
                    obj.meta.server_destroyed
                }).unwrap_or(false);
                if server_destroyed {
                    map.remove(self.id);
                }
            }
        }
    }

    pub(crate) fn equals(&self, other: &ProxyInner) -> bool {
        self.is_alive() && Arc::ptr_eq(&self.object.meta.alive, &other.object.meta.alive)
    }

    pub(crate) fn make_wrapper(&self, queue: &EventQueueInner) -> Result<ProxyInner, ()> {
        let mut wrapper = self.clone();
        wrapper.object.meta.buffer = queue.buffer.clone();
        Ok(wrapper)
    }

    pub(crate) fn is_implemented_with<I: Interface, Impl>(&self) -> bool
    where
        Impl: Implementation<Proxy<I>, I::Event> + 'static,
        I::Event: MessageGroup<Map = super::ProxyMap>,
    {
        self.object
            .meta
            .dispatcher
            .lock()
            .unwrap()
            .is::<super::ImplDispatcher<I, Impl>>()
    }

    pub(crate) fn child<I: Interface>(&self) -> NewProxyInner {
        self.child_versioned::<I>(self.object.version)
    }

    pub fn child_versioned<I: Interface>(&self, version: u32) -> NewProxyInner {
        let new_object = Object::from_interface::<I>(version, self.object.meta.child());
        let new_id = self.map.lock().unwrap().client_insert_new(new_object);
        NewProxyInner {
            map: self.map.clone(),
            connection: self.connection.clone(),
            id: new_id,
        }
    }
}

pub(crate) struct NewProxyInner {
    map: Arc<Mutex<ObjectMap<ObjectMeta>>>,
    connection: Arc<Mutex<Connection>>,
    id: u32,
}

impl NewProxyInner {
    pub(crate) fn from_id(
        id: u32,
        map: Arc<Mutex<ObjectMap<ObjectMeta>>>,
        connection: Arc<Mutex<Connection>>,
    ) -> Option<NewProxyInner> {
        if map.lock().unwrap().find(id).is_some() {
            Some(NewProxyInner { map, connection, id })
        } else {
            None
        }
    }

    /// Racy method, if called, must be called before any event ot this object
    /// is read from the socket, or it'll end up in the wrong queue...
    pub(crate) unsafe fn assign_queue(&self, queue: &EventQueueInner) {
        let _ = self.map.lock().unwrap().with(self.id, |obj| {
            obj.meta.buffer = queue.buffer.clone();
        });
    }

    // Invariants: Impl is either `Send` or we are on the same thread as the target event loop
    pub(crate) unsafe fn implement<I: Interface, Impl>(self, implementation: Impl) -> ProxyInner
    where
        Impl: Implementation<Proxy<I>, I::Event> + 'static,
        I::Event: MessageGroup<Map = super::ProxyMap>,
    {
        let object = self.map.lock().unwrap().with(self.id, |obj| {
            obj.meta.dispatcher = super::make_dispatcher(implementation);
            obj.clone()
        });

        let object = match object {
            Ok(obj) => obj,
            Err(()) => {
                // We are tyring to implement a non-existent object
                // This is either a bug in the lib (a NewProxy was created while it should not
                // have been possible) or an object was created and the server destroyed it
                // before it could be implemented.
                // Thus, we just create a dummy already-dead Proxy
                Object::from_interface::<I>(1, ObjectMeta::dead())
            }
        };

        ProxyInner {
            map: self.map,
            connection: self.connection,
            id: self.id,
            object,
        }
    }
}
