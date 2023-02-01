
use clap::{Parser, Subcommand};
use log::debug;
use std::env;
use std::fs;
use std::io;
use std::net;
use std::ops;
use std::os::fd::FromRawFd;
use std::os::unix;
use std::time;
use tokio;
use tokio_stream;
use warp::{self,Filter};

const PORT_RANGE: ops::RangeInclusive<usize> = 1..=65535;

fn port_in_range (s: &str)
    -> Result<u16, String>
{
    let port: usize = s.parse ().map_err (|_| format!("`{}` isn't a port number", s))?;
    if PORT_RANGE.contains(&port)
    {
        Ok(port as u16)
    }
    else
    {
        Err (format! ("port not in range {}-{}", PORT_RANGE.start (), PORT_RANGE.end ()))
    }
}

// based on https://github.com/stackabletech/secret-operator/pull/26/files
mod bind_private
{
    #[cfg(not(target_vendor="apple"))]
    use libc;
    use socket2::{self,Socket};
    #[cfg(not(target_vendor="apple"))]
    use std::os::unix::prelude::AsRawFd;
    use std::path;
    use tokio;

    /// Bind a Unix Domain Socket listener that is only accessible to the current user
    pub fn uds_bind_private (path: impl AsRef<path::Path>) -> Result<tokio::net::UnixListener, std::io::Error> {
        // Workaround for https://github.com/tokio-rs/tokio/issues/4422
        let socket = Socket::new (socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
	#[cfg(not(target_vendor="apple"))]
        unsafe {
            // Socket-level chmod is propagated to the file created by Socket::bind.
            // We need to chmod /before/ creating the file, because otherwise there is a brief window where
            // the file is world-accessible (unless restricted by the global umask).
            if libc::fchmod (socket.as_raw_fd (), libc::S_IRUSR | libc::S_IWUSR) == -1
            {
                return Err (std::io::Error::last_os_error ());
            }
        }
        socket.bind (&socket2::SockAddr::unix (path)?)?;
        socket.set_nonblocking (true)?;
        socket.listen (1024)?;
        tokio::net::UnixListener::from_std (socket.into ())
    }
}

pub async fn handler (mut rx_cancel: tokio::sync::broadcast::Receiver<()>)
    -> Result<impl warp::Reply, std::convert::Infallible>
{
    let sleep_msg = match env::var ("ECHO_SLEEP")
    {
        Ok (ss) => {
            let secs = ss.parse::<u64> ().unwrap ();
            if let Ok(_) = tokio::time::timeout (time::Duration::from_secs (secs), rx_cancel.recv ()).await
            {
                String::from ("Sleep was interupted")
            }
            else
            {
                format! ("woke after {}", secs)
            }
        },
        _ => { String::from ("") }
    };
    match env::var ("ECHO_ENV")
    {
        Ok (ee) => {
            match env::var (ee.clone ())
            {
                Ok (ev) => Ok (format! ("{}\n{}={}\nHello, World!", sleep_msg, ee, ev)),
                _ => Ok (format! ("{}\n{} not set\nHello, World!", sleep_msg, ee))
            }
        },
        _ => Ok (format! ("{}\nHello World!", sleep_msg))
    }
}


pub fn socket_activation_pong (handle: &tokio::runtime::Handle, tx_cancel: tokio::sync::broadcast::Sender<()>)
{
    handle.block_on (async move {
        // SAFETY: no other functions should call `from_raw_fd`, so there
        // is only one owner for the file descriptor.
        let ul = unsafe { unix::net::UnixListener::from_raw_fd (3) };
        match ul.set_nonblocking (true)
        {
            Ok (_) => {
                match tokio::net::UnixListener::from_std (ul)
                {
                    Ok (tl) => {
                        debug! ("obtained tokio listener");
                        let mut rx_shutdown = tx_cancel.subscribe ();
                        let routes = warp::any().map (move || tx_cancel.subscribe ()).and_then (handler);

                        warp::serve (routes)
                            .serve_incoming_with_graceful_shutdown (tokio_stream::wrappers::UnixListenerStream::new (tl), async move { rx_shutdown.recv ().await.ok (); })
                            .await;
                        debug! ("Shutdown");
                    },
                    Err (e) => {
                        debug! ("Error creating tokio listener: {:?}", e);
                    }
                }
            },
            Err (e) => { debug! ("Error setting unix listener to non_blocking: {:?}", e); }
        }
    });
}

pub fn socket_activation_proxy (handle: &tokio::runtime::Handle, socket_file_path: &str)
{
    handle.block_on (async move {
        // SAFETY: no other functions should call `from_raw_fd`, so there
        // is only one owner for the file descriptor.
        let ul = unsafe { net::TcpListener::from_raw_fd (3) };
        match ul.accept ()
        {
            Ok ((us_active, _socket_address)) => {
                debug! ("ul accepted connection");
                match tokio::net::TcpStream::from_std (us_active)
                {
                    Ok (mut ts_active) => {
                        debug! ("obtained tokio stream active");
                        match tokio::net::UnixStream::connect (socket_file_path).await
                        {
                            Ok (mut ts_uds) => {
                                debug! ("obtained tokio stream uds");
                                match tokio::io::copy_bidirectional (&mut ts_active, &mut ts_uds).await
                                {
                                    Ok ((ab, ba)) => { debug! ("bytes ab: {} bytes ba: {}", ab, ba); },
                                    Err (e) => { debug! ("ts stream copy error: {:?}", e); }
                                }
                            },
                            Err (e) => { debug! ("Error connecting tokio stream uds: {:?}", e); }
                        }
                        debug! ("Shutdown");
                    },
                    Err (e) => {
                        debug! ("Error creating tokio stream active: {:?}", e);
                    }
                }
            },
            Err (e) => { debug! ("Error accepting ul: {:?}", e); }
        }
    });
}

// Its blocking so hangs the program on ctl-c and only accepts one connection, but hopefully it
// will do for now
pub fn tcp_proxy (handle: &tokio::runtime::Handle, host: &str, port: u16, socket_file_path: &str)
{
    handle.block_on (async move {
        match net::TcpListener::bind ((host, port))
        {
            Ok (ul) => {
                match ul.accept ()
                {
                    Ok ((us_active, _socket_address)) => {
                        debug! ("ul accepted connection");
                        match tokio::net::TcpStream::from_std (us_active)
                        {
                            Ok (mut ts_active) => {
                                debug! ("obtained tokio stream active");
                                match tokio::net::UnixStream::connect (socket_file_path).await
                                {
                                    Ok (mut ts_uds) => {
                                        debug! ("obtained tokio stream uds");
                                        match tokio::io::copy_bidirectional (&mut ts_active, &mut ts_uds).await
                                        {
                                            Ok ((ab, ba)) => { debug! ("bytes ab: {} bytes ba: {}", ab, ba); },
                                            Err (e) => { debug! ("ts stream copy error: {:?}", e); }
                                        }
                                    },
                                    Err (e) => { debug! ("Error connecting tokio stream uds: {:?}", e); }
                                }
                                debug! ("Shutdown");
                            },
                            Err (e) => {
                                debug! ("Error creating tokio stream active: {:?}", e);
                            }
                        }
                    },
                    Err (e) => { debug! ("Error accepting ul: {:?}", e); }
                }
            },
            Err (e) => { debug! ("Error binding {}:{} {:?}", host, port, e); }
        }
    });
}

pub fn unix_pong (handle: &tokio::runtime::Handle, socket_file_path: &str, tx_cancel: tokio::sync::broadcast::Sender<()>)
{
    handle.block_on (async move {
        match fs::remove_file (socket_file_path)
        {
            Ok (_) => { debug! ("cleaned up existing socket file"); }
            Err (e) => {
                match e.kind ()
                {
                    io::ErrorKind::NotFound => {},
                    _ => { debug! ("Unknown io error: {:?}", e); }
                }
            }
        }
        match bind_private::uds_bind_private (socket_file_path)
        {
            Ok (tl) => {
                debug! ("obtained tokio listener");
                let mut rx_shutdown = tx_cancel.subscribe ();
                let routes = warp::any().map (move || tx_cancel.subscribe ()).and_then (handler);

                warp::serve (routes)
                    .serve_incoming_with_graceful_shutdown (tokio_stream::wrappers::UnixListenerStream::new (tl), async move { rx_shutdown.recv ().await.ok (); })
                    .await;
                debug! ("Server Shutdown");
            },
            Err (e) => {
                debug! ("Error creating tokio listener: {:?}", e);
            }
        }
    });
}


#[derive(Subcommand)]
enum Commands {
    ActivationPong,
    ActivationProxyUnix {
        socket_file_path: String
    },
    TcpProxyUnix {
        host: String,
        #[arg(value_parser = port_in_range)]
        port: u16,
        socket_file_path: String
    },
    UnixPong {
        socket_file_path: String
    }
}

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Cli {

    #[command(subcommand)]
    command: Commands,
}

fn main ()
{
    env_logger::init ();

    debug! ("hello lets listen to some sockets today");
    let rt = tokio::runtime::Builder::new_multi_thread ()
        .worker_threads (2)
        .enable_all ()
        .build ()
        .unwrap ();
    let args = Cli::parse ();

    let (tx, _) = tokio::sync::broadcast::channel::<()> (5);
    let tx_cancel = tx.clone ();

    rt.spawn (async move {
        // We don't need to loop as the only thing we will do is quit
        tokio::select! {
            _ = tokio::signal::ctrl_c () => { tx_cancel.send (()).ok ();debug! ("Got crl-c"); },
        }
    });

    match args.command
    {
        Commands::ActivationPong => socket_activation_pong (rt.handle (), tx),
        Commands::ActivationProxyUnix { socket_file_path } => socket_activation_proxy (rt.handle (), &socket_file_path),
        Commands::TcpProxyUnix { host, port, socket_file_path } => tcp_proxy (rt.handle (), &host, port, &socket_file_path),
        Commands::UnixPong { socket_file_path } => unix_pong (rt.handle (), &socket_file_path, tx)
    }
    debug! ("Shutting down");
}


