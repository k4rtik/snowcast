use std::collections::HashSet;
use std::io::{self, ErrorKind};
use std::rc::Rc;
use std::time::Duration;
use std::sync::mpsc;
use std::sync::mpsc::{Sender, Receiver};
use std::thread;
use std::fs::File;
use std::io::Read;
use std::net::{SocketAddr, IpAddr, Ipv4Addr};
use std::io::{Seek, SeekFrom};

use byteorder::{ByteOrder, BigEndian};
use commands::*;
use slab;
use mio::*;
use mio::tcp::*;
use mio::udp::*;

use connection::Connection;

type Slab<T> = slab::Slab<T, Token>;

type UdpAddress = (Ipv4Addr, u16);

enum Action {
    Add(UdpAddress),
    Remove(UdpAddress),
}

pub struct Server {
    // main socket for our server
    sock: TcpListener,

    // token of our server. we keep track of it here instead of doing `const SERVER = Token(0)`.
    token: Token,

    // a list of connections _accepted_ by our server
    conns: Slab<Connection>,

    // a list of events to process
    events: Events,

    // available stations on this server
    stations: Vec<String>,

    channels: Vec<Sender<Action>>,
}

fn broadcast_channel(rx: Receiver<Action>, filename: String) {
    let mut recipients = HashSet::<UdpAddress>::new();
    let mut f = File::open(filename).unwrap(); // XXX or panic!
    let addr = ("0.0.0.0:".to_string() + "0")
        .parse::<SocketAddr>()
        .unwrap();

    let sock = UdpSocket::bind(&addr).unwrap();
    loop {
        match rx.try_recv() {
            Ok(action) => {
                match action {
                    Action::Add(udpaddress) => {
                        debug!("adding: {:?}", udpaddress);
                        recipients.insert(udpaddress);
                    }
                    Action::Remove(udpaddress) => {
                        debug!("removing: {:?}", udpaddress);
                        recipients.remove(&udpaddress);
                    }
                }
            }
            Err(_) => (),
            // Err(mpsc::TryRecvError::Empty) => 65535,
            // Err(mpsc::TryRecvError::Disconnected) => return,
        };

        let mut buffer = [0; 1024];
        let len = match f.read(&mut buffer[..]) {
            Ok(0) => {
                f.seek(SeekFrom::Start(0)).unwrap();
                0
            }
            Ok(n) => n,
            _ => {
                println!("Error reading file!");
                return;
            }
        };

        for recipient in &recipients {
            debug!("rec: {:?}", recipient);
            let dest = SocketAddr::new(IpAddr::V4(recipient.0), recipient.1);
            sock.send_to(&buffer[0..len], &dest).unwrap();
        }
        thread::sleep(Duration::new(0, 62_500_000)); // 62.5ms
    }
}

impl Server {
    pub fn new(sock: TcpListener, stations: Vec<String>) -> Server {
        let mut channels = Vec::<Sender<Action>>::new();
        for i in 0..stations.len() {
            let station = stations[i].clone();
            let (tx, rx): (Sender<Action>, Receiver<Action>) = mpsc::channel();
            thread::spawn(move || broadcast_channel(rx, station));
            channels.push(tx);
        }

        Server {
            sock: sock,

            // Give our server token a number much larger than our slab capacity. The slab used to
            // track an internal offset, but does not anymore.
            token: Token(10_000_000),

            // SERVER is Token(1), so start after that
            // we can deal with a max of 126 connections
            conns: Slab::with_capacity(128),

            // list of events from the poller that the server needs to process
            events: Events::with_capacity(1024),

            // vector of available stations on this server
            stations: stations,

            channels: channels,
        }
    }

    pub fn run(&mut self, poll: &mut Poll) -> io::Result<()> {

        try!(self.register(poll));

        info!("Server run loop starting...");
        loop {
            let cnt = try!(poll.poll(&mut self.events, Some(Duration::from_millis(100))));

            let mut i = 0;

            // trace!("processing events... cnt={}; len={}",
            //        cnt,
            //        self.events.len());

            // Iterate over the notifications. Each event provides the token
            // it was registered with (which usually represents, at least, the
            // handle that the event is about) as well as information about
            // what kind of event occurred (readable, writable, signal, etc.)
            while i < cnt {
                let event = self.events.get(i).expect("Failed to get event");

                trace!("event={:?}; idx={:?}", event, i);
                self.ready(poll, event.token(), event.kind());

                i += 1;
            }

            self.tick(poll);
        }
    }

    /// Register Server with the poller.
    ///
    /// This keeps the registration details neatly tucked away inside of our implementation.
    pub fn register(&mut self, poll: &mut Poll) -> io::Result<()> {
        poll.register(&self.sock, self.token, Ready::readable(), PollOpt::edge())
            .or_else(|e| {
                error!("Failed to register server {:?}, {:?}", self.token, e);
                Err(e)
            })
    }

    fn tick(&mut self, poll: &mut Poll) {
        // trace!("Handling end of tick");

        let mut reset_tokens = Vec::new();

        for c in self.conns.iter_mut() {
            if c.is_reset() {
                reset_tokens.push(c.token);
            } else if c.is_idle() {
                c.reregister(poll)
                    .unwrap_or_else(|e| {
                        warn!("Reregister failed {:?}", e);
                        c.mark_reset();
                        reset_tokens.push(c.token);
                    });
            }
        }

        for token in reset_tokens {
            let current_channel = self.find_connection_by_token(token)
                .get_current_channel() as usize;
            let ip = self.find_connection_by_token(token).get_addr();
            let udp_port = self.find_connection_by_token(token).get_udp_port();
            if current_channel < self.channels.len() {
                debug!("sending message to remove port: {}", udp_port);
                self.channels[current_channel].send(Action::Remove((ip, udp_port))).unwrap();
            }

            match self.conns.remove(token) {
                Some(_c) => {
                    debug!("reset connection; token={:?}", token);
                }
                None => {
                    warn!("Unable to remove connection for {:?}", token);
                }
            }
        }
    }

    fn ready(&mut self, poll: &mut Poll, token: Token, event: Ready) {
        debug!("{:?} event = {:?}", token, event);

        if event.is_error() {
            warn!("Error event for {:?}", token);
            self.find_connection_by_token(token).mark_reset();
            return;
        }

        if event.is_hup() {
            trace!("Hup event for {:?}", token);
            self.find_connection_by_token(token).mark_reset();
            println!("{:?}: client closed connection", token);
            return;
        }

        // We never expect a write event for our `Server` token . A write event for any other token
        // should be handed off to that connection.
        if event.is_writable() {
            trace!("Write event for {:?}", token);
            assert!(self.token != token, "Received writable event for Server");

            let conn = self.find_connection_by_token(token);

            if conn.is_reset() {
                info!("{:?} has already been reset", token);
                return;
            }

            conn.writable()
                .unwrap_or_else(|e| {
                    warn!("Write event failed for {:?}, {:?}", token, e);
                    conn.mark_reset();
                });
        }

        // A read event for our `Server` token means we are establishing a new connection. A read
        // event for any other token should be handed off to that connection.
        if event.is_readable() {
            trace!("Read event for {:?}", token);
            if self.token == token {
                self.accept(poll);
            } else {

                if self.find_connection_by_token(token).is_reset() {
                    info!("{:?} has already been reset", token);
                    return;
                }

                self.readable(token)
                    .unwrap_or_else(|e| {
                        warn!("Read event failed for {:?}: {:?}", token, e);
                        self.find_connection_by_token(token).mark_reset();
                    });
            }
        }

        if self.token != token {
            self.find_connection_by_token(token).mark_idle();
        }
    }

    /// Accept a _new_ client connection.
    ///
    /// The server will keep track of the new connection and forward any events from the poller
    /// to this connection.
    fn accept(&mut self, poll: &mut Poll) {
        debug!("server accepting new socket");

        // XXX loop because we are not oneshot anymore, but how does this loop exit?
        loop {
            // Log an error if there is no socket, but otherwise move on so we do not tear down the
            // entire server.
            let (sock, ip) = match self.sock.accept() {
                Ok((sock, SocketAddr::V4(ip))) => (sock, ip),
                Err(e) => {
                    if e.kind() == ErrorKind::WouldBlock {
                        debug!("accept encountered WouldBlock");
                    } else {
                        error!("Failed to accept new socket, {:?}", e);
                    }
                    return;
                }
                _ => {
                    error!("IPv6 not supported");
                    return;
                }
            };
            // let SocketAddr::V4(ip) = addr;
            let token = match self.conns.vacant_entry() {
                Some(entry) => {
                    debug!("registering {:?} with poller", entry.index());
                    let c = Connection::new(sock, entry.index(), ip.ip().clone());
                    entry.insert(c).index()
                }
                None => {
                    error!("Failed to insert connection into slab");
                    return;
                }
            };

            match self.find_connection_by_token(token).register(poll) {
                Ok(_) => {
                    println!("{:?}: new client connected; expecting HELLO", token);
                }
                Err(e) => {
                    error!("Failed to register {:?} connection with poller, {:?}",
                           token,
                           e);
                    self.conns.remove(token);
                }
            }
        }
    }

    fn disconnect_with_invalid_command(&mut self, token: Token, reply: &str) {
        let reply_string = reply.to_string();
        let reply_string_size = reply_string.len();
        let mut invalidbuf: Vec<u8> = vec![0; 2];
        unsafe {
            invalidbuf.set_len(2);
        }
        invalidbuf[0] = 2; // reply_type
        invalidbuf[1] = reply_string_size as u8;
        let mut reply_string_vec = reply_string.into_bytes();
        invalidbuf.append(&mut reply_string_vec);
        self.find_connection_by_token(token)
            .send_message(Rc::new(invalidbuf.to_vec()))
            .ok();
        self.find_connection_by_token(token).mark_to_be_removed();
    }

    /// Forward a readable event to an established connection.
    ///
    /// Connections are identified by the token provided to us from the poller. Once a read has
    /// finished, push the receive buffer into the all the existing connections so we can
    /// broadcast.
    fn readable(&mut self, token: Token) -> io::Result<()> {
        debug!("server conn readable; token={:?}", token);

        while let Some(command) = try!(self.find_connection_by_token(token).readable()) {
            match command {
                ServerCommand::Hello { udp_port, .. } => {
                    info!("udp_port: {}", udp_port);
                    if self.find_connection_by_token(token).is_handshake_done() {
                        println!("{:?}: re-received HELLO, sending INVALID_COMMAND; closing \
                                  connection",
                                 token);
                        self.disconnect_with_invalid_command(token,
                                                             "Handshake already done but server \
                                                              re-received HELLO");
                    } else {
                        self.find_connection_by_token(token).set_udp_port(udp_port);
                        println!("{:?}: HELLO received; sending WELCOME, expecting SET_STATION",
                                 token);
                        let mut welcomebuf: Vec<u8> = vec![0; 3];
                        unsafe {
                            welcomebuf.set_len(3);
                        }
                        debug!("Station Count: {}", self.stations.len());
                        BigEndian::write_u16(&mut welcomebuf[1..], self.stations.len() as u16);
                        debug!("{:?}", welcomebuf);
                        self.find_connection_by_token(token)
                            .send_message(Rc::new(welcomebuf.to_vec()))
                            .ok();
                        self.find_connection_by_token(token).mark_handshake_done();
                    }
                }
                ServerCommand::SetStation { station_number, .. } => {
                    let station_number = station_number as usize;
                    if station_number >= self.stations.len() {
                        println!("{:?}: received request for invalid station: {}, \
                                  sending INVALID_COMMAND; closing connection",
                                 token,
                                 station_number);
                        self.disconnect_with_invalid_command(token,
                                                             "server received a SET_STATION \
                                                              command with an invalid station \
                                                              number");
                    } else {
                        println!("{:?}: received SET_STATION to station {}",
                                 token,
                                 station_number);

                        let current_channel = self.find_connection_by_token(token)
                            .get_current_channel();
                        let ip = self.find_connection_by_token(token).get_addr();
                        let udp_port = self.find_connection_by_token(token).get_udp_port();
                        if current_channel < self.stations.len() as u16 {
                            debug!("sending message to remove port: {}", udp_port);
                            self.channels[current_channel as usize]
                                .send(Action::Remove((ip, udp_port)))
                                .unwrap();
                        }
                        self.channels[station_number].send(Action::Add((ip, udp_port))).unwrap();
                        self.find_connection_by_token(token)
                            .set_current_channel(station_number as u16);

                        let song_name_size = self.stations[station_number].len();
                        let mut announcebuf: Vec<u8> = vec![0; 2];
                        unsafe {
                            announcebuf.set_len(2);
                        }
                        announcebuf[0] = 1; // reply_type
                        announcebuf[1] = song_name_size as u8;
                        let song_name = self.stations[station_number].clone();
                        debug!("song_name: {:?}", song_name);
                        let mut song_name_vec = song_name.into_bytes();
                        announcebuf.append(&mut song_name_vec);
                        self.find_connection_by_token(token)
                            .send_message(Rc::new(announcebuf.to_vec()))
                            .ok();
                        debug!("Sending songname: {}",
                               String::from_utf8(announcebuf[2..].to_vec()).unwrap());
                    }
                }
                _ => {
                    println!("{:?}: received unknown command type, sending INVALID_COMMAND; \
                              closing connection",
                             token);
                    self.disconnect_with_invalid_command(token,
                                                         "server received an unknown command");
                }
            }
        }

        Ok(())
    }

    /// Find a connection in the slab using the given token.
    fn find_connection_by_token<'a>(&'a mut self, token: Token) -> &'a mut Connection {
        &mut self.conns[token]
    }
}
