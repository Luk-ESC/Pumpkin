#![allow(clippy::await_holding_refcell_ref)]

use mio::net::TcpListener;
use mio::{Events, Interest, Poll, Token};
use std::io::{self};

use client::Client;
use commands::handle_command;
use config::AdvancedConfiguration;

use std::{collections::HashMap, rc::Rc, thread};

use client::interrupted;
use config::BasicConfiguration;
use server::Server;

// Setup some tokens to allow us to identify which event is for which socket.

pub mod client;
pub mod commands;
pub mod config;
pub mod entity;
pub mod proxy;
pub mod rcon;
pub mod server;
pub mod util;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(not(target_os = "wasi"))]
fn main() -> io::Result<()> {
    use entity::player::Player;
    use pumpkin_core::text::{color::NamedColor, TextComponent};

    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();
    #[cfg(feature = "dhat-heap")]
    println!("Using a memory profiler");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    ctrlc::set_handler(|| {
        log::warn!(
            "{}",
            TextComponent::text("Stopping Server")
                .color_named(NamedColor::Red)
                .to_pretty_console()
        );
        std::process::exit(0);
    })
    .unwrap();
    // ensure rayon is built outside of tokio scope
    rayon::ThreadPoolBuilder::new().build_global().unwrap();
    rt.block_on(async {
        const SERVER: Token = Token(0);
        use std::{cell::RefCell, time::Instant};

        use rcon::RCONServer;

        let time = Instant::now();
        let basic_config = BasicConfiguration::load("configuration.toml");

        let advanced_configuration = AdvancedConfiguration::load("features.toml");

        simple_logger::SimpleLogger::new().init().unwrap();

        // Create a poll instance.
        let mut poll = Poll::new()?;
        // Create storage for events.
        let mut events = Events::with_capacity(128);

        // Setup the TCP server socket.

        let addr = format!(
            "{}:{}",
            basic_config.server_address, basic_config.server_port
        )
        .parse()
        .unwrap();

        let mut listener = TcpListener::bind(addr)?;

        // Register the server with poll we can receive events for it.
        poll.registry()
            .register(&mut listener, SERVER, Interest::READABLE)?;

        // Unique token for each incoming connection.
        let mut unique_token = Token(SERVER.0 + 1);

        let use_console = advanced_configuration.commands.use_console;
        let rcon = advanced_configuration.rcon.clone();

        let mut clients: HashMap<Token, Client> = HashMap::new();
        let mut players: HashMap<Rc<Token>, Rc<RefCell<Player>>> = HashMap::new();

        let mut server = Server::new((basic_config, advanced_configuration));
        log::info!("Started Server took {}ms", time.elapsed().as_millis());
        log::info!("You now can connect to the server, Listening on {}", addr);

        if use_console {
            thread::spawn(move || {
                let stdin = std::io::stdin();
                loop {
                    let mut out = String::new();
                    stdin
                        .read_line(&mut out)
                        .expect("Failed to read console line");

                    if !out.is_empty() {
                        handle_command(&mut commands::CommandSender::Console, &out);
                    }
                }
            });
        }
        if rcon.enabled {
            tokio::spawn(async move {
                RCONServer::new(&rcon).await.unwrap();
            });
        }
        loop {
            if let Err(err) = poll.poll(&mut events, None) {
                if interrupted(&err) {
                    continue;
                }
                return Err(err);
            }

            for event in events.iter() {
                match event.token() {
                    SERVER => loop {
                        // Received an event for the TCP server socket, which
                        // indicates we can accept an connection.
                        let (mut connection, address) = match listener.accept() {
                            Ok((connection, address)) => (connection, address),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                // If we get a `WouldBlock` error we know our
                                // listener has no more incoming connections queued,
                                // so we can return to polling and wait for some
                                // more.
                                break;
                            }
                            Err(e) => {
                                // If it was any other kind of error, something went
                                // wrong and we terminate with an error.
                                return Err(e);
                            }
                        };
                        if let Err(e) = connection.set_nodelay(true) {
                            log::warn!("failed to set TCP_NODELAY {e}");
                        }

                        log::info!("Accepted connection from: {}", address);

                        let token = next(&mut unique_token);
                        poll.registry().register(
                            &mut connection,
                            token,
                            Interest::READABLE.add(Interest::WRITABLE),
                        )?;
                        let rc_token = Rc::new(token);
                        let client = Client::new(Rc::clone(&rc_token), connection, addr);
                        clients.insert(token, client);
                    },

                    token => {
                        // Poll Players
                        let done = if let Some(player) = players.get_mut(&token) {
                            let mut player = player.borrow_mut();
                            player.client.poll(event).await;
                            player.process_packets(&mut server);
                            player.client.closed
                        } else {
                            false
                        };

                        if done {
                            if let Some(player) = players.remove(&token) {
                                server.remove_player(&token);
                                let mut player = player.borrow_mut();
                                poll.registry().deregister(&mut player.client.connection)?;
                            }
                        }

                        // Poll current Clients (non players)
                        // Maybe received an event for a TCP connection.
                        let (done, make_player) = if let Some(client) = clients.get_mut(&token) {
                            client.poll(event).await;
                            client.process_packets(&mut server).await;
                            (client.closed, client.make_player)
                        } else {
                            // Sporadic events happen, we can safely ignore them.
                            (false, false)
                        };
                        if done || make_player {
                            if let Some(mut client) = clients.remove(&token) {
                                if done {
                                    poll.registry().deregister(&mut client.connection)?;
                                } else if make_player {
                                    let token = client.token.clone();
                                    let player = server.add_player(token.clone(), client);
                                    players.insert(token, player.clone());
                                    let mut player = player.borrow_mut();
                                    server.spawn_player(&mut player).await;
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

fn next(current: &mut Token) -> Token {
    let next = current.0;
    current.0 += 1;
    Token(next)
}

#[cfg(target_os = "wasi")]
fn main() {
    panic!("can't bind to an address with wasi")
}
