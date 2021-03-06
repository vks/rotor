use std::marker::PhantomData;
use std::io;
use std::io::ErrorKind::{WouldBlock, Interrupted};

use netbuf::Buf;
use time::SteadyTime;
use mio::{EventSet, PollOpt};

use super::StreamSocket as Socket;
use super::accept::Init;
use handler::{Registrator};
use {Async, EventMachine};

pub struct Timeout(pub SteadyTime);

struct Inner<S: Socket> {
    socket: S,
    inbuf: Buf,
    outbuf: Buf,
    writable: bool,
    readable: bool,
}

pub struct Stream<C, S: Socket, P: Protocol<C>>
    (Inner<S>, P, PhantomData<*mut C>);

pub struct Transport<'a> {
    inbuf: &'a mut Buf,
    outbuf: &'a mut Buf,
}


impl<S: Socket> Inner<S> {
    fn transport(&mut self) -> Transport {
        Transport {
            inbuf: &mut self.inbuf,
            outbuf: &mut self.outbuf,
        }
    }
}

impl<C, S: Socket, P: Protocol<C>> Init<S, C> for Stream<C, S, P> {
    fn accept(mut conn: S, context: &mut C) -> Option<Self>
    {
        let protocol = match Protocol::accepted(&mut conn, context) {
            Some(x) => x,
            None => return None
        };

        Some(Stream(Inner {
            socket: conn,
            inbuf: Buf::new(),
            outbuf: Buf::new(),
            readable: false,
            writable: true,   // Accepted socket is immediately writable
        }, protocol, PhantomData))
    }
}

impl<C, S: Socket, P: Protocol<C>> EventMachine<C> for Stream<C, S, P> {
    fn ready(self, evset: EventSet, context: &mut C)
        -> Async<Self, Option<Self>>
    {
        let Stream(mut stream, fsm, _) = self;
        let mut monad = Async::Continue(fsm, ());
        if evset.is_writable() && stream.outbuf.len() > 0 {
            stream.writable = true;
            while stream.outbuf.len() > 0 {
                match stream.outbuf.write_to(&mut stream.socket) {
                    Ok(0) => { // Connection closed
                        monad.done(|fsm| fsm.eof_received(context));
                        return Async::Stop;
                    }
                    Ok(_) => {
                        monad = async_try!(monad.and_then(|f| {
                            f.data_transferred(
                                &mut stream.transport(), context)
                        }));
                    }
                    Err(ref e) if e.kind() == WouldBlock => {
                        stream.writable = false;
                        break;
                    }
                    Err(ref e) if e.kind() == Interrupted =>  { continue; }
                    Err(e) => {
                        monad.done(|fsm| fsm.error_happened(e, context));
                        return Async::Stop;
                    }
                }
            }
        }
        if evset.is_readable() {
            stream.readable = true;
            loop {
                match stream.inbuf.read_from(&mut stream.socket) {
                    Ok(0) => { // Connection closed
                        monad.done(|fsm| fsm.eof_received(context));
                        return Async::Stop;
                    }
                    Ok(_) => {
                        monad = async_try!(monad.and_then(|f| {
                            f.data_received(
                                &mut stream.transport(), context)
                        }));
                    }
                    Err(ref e) if e.kind() == WouldBlock => {
                        stream.readable = false;
                        break;
                    }
                    Err(ref e) if e.kind() == Interrupted =>  { continue; }
                    Err(e) => {
                        monad.done(|fsm| fsm.error_happened(e, context));
                        return Async::Stop;
                    }
                }
            }
        }
        if stream.writable && stream.outbuf.len() > 0 {
            while stream.outbuf.len() > 0 {
                match stream.outbuf.write_to(&mut stream.socket) {
                    Ok(0) => { // Connection closed
                        monad.done(|fsm| fsm.eof_received(context));
                        return Async::Stop;
                    }
                    Ok(_) => {
                        monad = async_try!(monad.and_then(|f| {
                            f.data_transferred(
                                &mut stream.transport(), context)
                        }));
                    }
                    Err(ref e) if e.kind() == WouldBlock => {
                        stream.writable = false;
                        break;
                    }
                    Err(ref e) if e.kind() == Interrupted =>  { continue; }
                    Err(e) => {
                        monad.done(|fsm| fsm.error_happened(e, context));
                        return Async::Stop;
                    }
                }
            }
        }
        monad
        .map(|fsm| Stream(stream, fsm, PhantomData))
        .map_result(|()| None)
    }

    fn register(self, reg: &mut Registrator) -> Async<Self, ()> {
        reg.register(&self.0.socket, EventSet::all(), PollOpt::edge());
        Async::Continue(self, ())
    }

    fn timeout(self, context: &mut C) -> Async<Self, Option<Self>> {
        let Stream(stream, fsm, _) = self;
        async_try!(fsm.timeout(context))
        .map(|fsm| Stream(stream, fsm, PhantomData))
        .map_result(|()| None)
    }

    fn wakeup(self, context: &mut C) -> Async<Self, Option<Self>> {
        let Stream(stream, fsm, _) = self;
        async_try!(fsm.wakeup(context))
        .map(|fsm| Stream(stream, fsm, PhantomData))
        .map_result(|()| None)
    }
}

pub trait Protocol<C>: Sized {
    fn accepted<S: Socket>(conn: &mut S, context: &mut C)
        -> Option<Self>;
    fn data_received(self, trans: &mut Transport, ctx: &mut C)
        -> Async<Self, ()>;
    fn data_transferred(self, _trans: &mut Transport, _ctx: &mut C)
        -> Async<Self, ()> {
        Async::Continue(self, ())
    }
    // TODO(tailhook) some error object should be here
    fn error_happened(self, _err: io::Error, _ctx: &mut C) {}
    fn eof_received(self, _ctx: &mut C) {}

    fn timeout(self, _context: &mut C) -> Async<Self, ()> {
        Async::Continue(self, ())
    }
    fn wakeup(self, _context: &mut C) -> Async<Self, ()> {
        Async::Continue(self, ())
    }
}

impl<'a> Transport<'a> {
    pub fn new(inbuf: &'a mut  Buf, outbuf: &'a mut Buf) -> Transport<'a> {
        Transport {
            inbuf: inbuf,
            outbuf: outbuf,
        }
    }
    pub fn input<'x>(&'x mut self) -> &'x mut Buf {
        self.inbuf
    }
    pub fn output<'x>(&'x mut self) -> &'x mut Buf {
        self.outbuf
    }
}

#[test]
fn test_transport() {
    let mut inbuf = Buf::new();
    inbuf.extend(b"spam");
    let mut outbuf = Buf::new();
    {
        let mut transport = Transport::new(&mut inbuf, &mut outbuf);
        let transport_input = transport.input();
        transport_input.extend(b"foobar");
        transport_input.consume(7);
    }
    assert_eq!(&inbuf[..], b"bar");
}
