
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

mod helper
{
    use std::io;
    use thiserror::Error;

    #[derive(Error, Debug)]
    pub enum PublicError
    {
        #[error("IOError: {0}")]
        IOError (io::Error)
    }

    impl From<io::Error> for PublicError
    {
        fn from (err: io::Error)
        -> PublicError
        {
            PublicError::IOError (err)
        }
    }
}

// based on https://github.com/stackabletech/secret-operator/pull/26/files
mod bind_private
{
    #[cfg(not(target_vendor="apple"))]
    use libc;
    use rand::Rng;
    use socket2::{self,Socket};
    use std::env;
    use std::fs;
    #[cfg(not(target_vendor="apple"))]
    use std::os::unix::prelude::AsRawFd;
    use std::path;
    use std::process;
    use tokio;

    pub fn new_socket_file_path ()
        -> Result<String, std::io::Error>
    {
        let mut rng = rand::thread_rng();
        let socket_dir = env::var ("XDG_RUNTIME_DIR").expect ("No XDG_RUNTIME_DIR set");
        let mut path_iter = (0..5).map (|_x| {
            let path = format! ("{}/{}_{}", socket_dir, process::id (), rng.gen::<usize> ());
            //let path = format! ("{}/{}", socket_dir, x);
            (path.clone (), fs::File::options().read(true).write(true).create_new(true).open (path))
        })
        .skip_while (|x| x.1.is_err ());
        match path_iter.next ()
        {
            Some ( (s, _f) ) => Ok (s),
            None => Err (std::io::Error::new (std::io::ErrorKind::Other, "Failed to create a new file"))
        }
    }

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

// warp will accept the connection, but we recieve an already accepted connection
// therefore simply forward to an unaccepted stream?
pub fn socket_activation_accepted_pong (handle: &tokio::runtime::Handle, tx_cancel: tokio::sync::broadcast::Sender<()>)
{
    handle.block_on (async move {
        // SAFETY: no other functions should call `from_raw_fd`, so there
        // is only one owner for the file descriptor.
        let ts = unsafe { net::TcpStream::from_raw_fd (3) };
        match ts.set_nonblocking (true)
        {
            Ok (_) => {
                match tokio::net::TcpStream::from_std (ts)
                {
                    Ok (mut tnts_accepted) => {
                        debug! ("obtained tokio tcp stream");
                        let unaccepted_socket_file_path = bind_private::new_socket_file_path ().expect ("Failed to obtain tmp socket file");
                        let unaccepted_socket_file_path_copy = unaccepted_socket_file_path.clone ();
                        debug! ("unaccepted_socket_file_path: {}", unaccepted_socket_file_path);
                        match fs::remove_file (unaccepted_socket_file_path.clone ())
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
                        match bind_private::uds_bind_private (unaccepted_socket_file_path)
                        {
                            Ok (tnul) => {
                                let handle = tokio::spawn(async move {
                                    let mut ts_uds = tokio::net::UnixStream::connect (unaccepted_socket_file_path_copy).await?;
                                    let (ab, ba) = tokio::io::copy_bidirectional (&mut tnts_accepted, &mut ts_uds).await?;
                                    debug! ("bytes ab: {} bytes ba: {}", ab, ba);
                                    Ok::<(), helper::PublicError> (())
                                });
                                let mut rx_shutdown = tx_cancel.subscribe ();
                                let routes = warp::any().map (move || tx_cancel.subscribe ()).and_then (handler);

                                warp::serve (routes)
                                    .serve_incoming_with_graceful_shutdown (tokio_stream::wrappers::UnixListenerStream::new (tnul), async move { rx_shutdown.recv ().await.ok (); })
                                    .await;
                                debug! ("Shutdown");
                                match handle.await
                                {
                                    Ok (_) => {},
                                    Err (e) => { debug! ("Error copying to unix socket: {:?}", e); }
                                }
                            },
                            Err (e) => {
                                debug! ("Error creating tokio unix listener: {:?}", e);
                            }
                        }
                    },
                    Err (e) => {
                        debug! ("Error creating tokio tcp listener: {:?}", e);
                    }
                }
            },
            Err (e) => { debug! ("Error setting tcp stream to non_blocking: {:?}", e); }
        }
    });
}

pub fn socket_activation_unix_pong (handle: &tokio::runtime::Handle, tx_cancel: tokio::sync::broadcast::Sender<()>)
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
                    Ok (tnul) => {
                        debug! ("obtained tokio listener");
                        let mut rx_shutdown = tx_cancel.subscribe ();
                        let routes = warp::any().map (move || tx_cancel.subscribe ()).and_then (handler);

                        warp::serve (routes)
                            .serve_incoming_with_graceful_shutdown (tokio_stream::wrappers::UnixListenerStream::new (tnul), async move { rx_shutdown.recv ().await.ok (); })
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

pub fn socket_activation_proxy (handle: &tokio::runtime::Handle, socket_file_path: &str, tx_cancel: tokio::sync::broadcast::Sender<()>)
{
    handle.block_on (async move {
        // SAFETY: no other functions should call `from_raw_fd`, so there
        // is only one owner for the file descriptor.
        let ul = unsafe { net::TcpListener::from_raw_fd (3) };

        match async {
            let mut rx_shutdown = tx_cancel.subscribe ();
            ul.set_nonblocking (true)?;
            let tl = tokio::net::TcpListener::from_std (ul)?;
            loop {
                match tokio::select! {
                    _ = rx_shutdown.recv () => { Err ("Got crl-c") },
                    rs = tl.accept () => {
                        let (mut ts_active, _socket_address) = rs?;
                        let mut ts_uds = tokio::net::UnixStream::connect (socket_file_path).await?;
                        debug! ("obtained tokio stream uds");
                        let (ab, ba) = tokio::io::copy_bidirectional (&mut ts_active, &mut ts_uds).await?;
                        debug! ("bytes ab: {} bytes ba: {}", ab, ba);
                        Ok (())
                    }
                }
                {
                    Ok (_) => {},
                    Err (e) => { debug! ("Accept loop shutdown: {:?}", e);break; }
                }
            }
            Result::Ok::<(), helper::PublicError> (())
        }.await
        {
            Ok (_) => {},
            Err (e) => { debug! ("Failed to proxy: {:?}", e); }
        }
    });
}

// Its blocking so hangs the program on ctl-c and only accepts one connection, but hopefully it
// will do for now
pub fn tcp_proxy (handle: &tokio::runtime::Handle, host: &str, port: u16, socket_file_path: &str, tx_cancel: tokio::sync::broadcast::Sender<()>)
{
    handle.block_on (async move {
        match async {
            let mut rx_shutdown = tx_cancel.subscribe ();
            let ul = net::TcpListener::bind ((host, port))?;
            ul.set_nonblocking (true)?;
            let tl = tokio::net::TcpListener::from_std (ul)?;
            loop {
                match tokio::select! {
                    _ = rx_shutdown.recv () => { Err ("Got crl-c") },
                    rs = tl.accept () => {
                        let (mut ts_active, _socket_address) = rs?;
                        debug! ("tl accepted connection");
                        let mut ts_uds = tokio::net::UnixStream::connect (socket_file_path).await?;
                        debug! ("obtained tokio stream uds");
                        let (ab, ba) = tokio::io::copy_bidirectional (&mut ts_active, &mut ts_uds).await?;
                        debug! ("bytes ab: {} bytes ba: {}", ab, ba);
                        Ok (())
                    }
                }
                {
                    Ok (_) => {},
                    Err (e) => { debug! ("Accept loop shutdown: {:?}", e);break; }
                }
            }
            Result::Ok::<(), helper::PublicError> (())
        }.await
        {
            Ok (_) => {},
            Err (e) => { debug! ("Failed to proxy: {:?}", e); }
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

pub fn unix_proxy (handle: &tokio::runtime::Handle, socket_file_path: &str, host: &str, port: u16, tx_cancel: tokio::sync::broadcast::Sender<()>)
{
    handle.block_on (async move {
        match async {
            let ul = unix::net::UnixListener::bind (socket_file_path)?;
            ul.set_nonblocking (true)?;
            let tnul = tokio::net::UnixListener::from_std (ul)?;
            debug! ("obtained tokio listener");
            let mut rx_shutdown = tx_cancel.subscribe ();

            loop {
                match tokio::select! {
                    _ = rx_shutdown.recv () => { Err ("Got crl-c") },
                    rs = tnul.accept () => {
                        let (mut us_active, _socket_address) = rs?;
                        debug! ("ul accepted connection");
                        let connection_url = format! ("{}:{}", host, port);
                        let mut ts_ts = tokio::net::TcpStream::connect (connection_url).await?;
                        let (ab, ba) = tokio::io::copy_bidirectional (&mut us_active, &mut ts_ts).await?;
                        debug! ("bytes ab: {} bytes ba: {}", ab, ba);
                        Ok (())
                    }
                }
                {
                    Ok (_) => {},
                    Err (e) => { debug! ("Accept loop shutdown: {:?}", e);break; }
                }
            }
            Result::Ok::<(), helper::PublicError> (())
        }.await
        {
            Ok (_) => {},
            Err (e) => { debug! ("Failed to proxy: {:?}", e); }
        }
    });
}

#[derive(Subcommand)]
enum Commands {
    ActivationPong,
    ActivationPongAccepted,
    ActivationPongUnix,
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
    },
    UnixProxyTcp {
        socket_file_path: String,
        host: String,
        #[arg(value_parser = port_in_range)]
        port: u16
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
        Commands::ActivationPongAccepted => socket_activation_accepted_pong (rt.handle (), tx),
        Commands::ActivationPongUnix => socket_activation_unix_pong (rt.handle (), tx),
        Commands::ActivationProxyUnix { socket_file_path } => socket_activation_proxy (rt.handle (), &socket_file_path, tx),
        Commands::TcpProxyUnix { host, port, socket_file_path } => tcp_proxy (rt.handle (), &host, port, &socket_file_path, tx),
        Commands::UnixPong { socket_file_path } => unix_pong (rt.handle (), &socket_file_path, tx),
        Commands::UnixProxyTcp { socket_file_path, host, port } => unix_proxy (rt.handle (), &socket_file_path, &host, port, tx)
    }
    debug! ("Shutting down");
}


