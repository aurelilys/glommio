/// Example on how to use the Hyper server in !Send mode.
/// The clients are harder, see https://github.com/hyperium/hyper/issues/2341 for details
///
/// Essentially what we do is we wrap our types around the Tokio traits. The
/// `!Send` limitation makes it harder to deal with high level hyper primitives,
/// but it works in the end.
mod hyper_compat {
    use futures_lite::{AsyncRead, AsyncWrite, Future};
    use hyper::{rt::ReadBufCursor, server::conn::http1, service::service_fn};
    use std::{
        mem::MaybeUninit,
        net::SocketAddr,
        pin::Pin,
        task::{Context, Poll},
    };

    use glommio::{
        enclose,
        net::{TcpListener, TcpStream},
        sync::Semaphore,
    };
    use hyper::{Request, Response};
    use std::{io, rc::Rc};

    struct HyperStream(pub TcpStream);
    impl hyper::rt::Read for HyperStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context,
            mut buf: ReadBufCursor<'_>,
        ) -> Poll<io::Result<()>> {
            let inner = unsafe { &mut *(buf.as_mut() as *mut [MaybeUninit<u8>] as *mut [u8]) };
            Pin::new(&mut self.0).poll_read(cx, inner).map(|n| {
                if let Ok(n) = n {
                    unsafe { buf.advance(n) };
                }
                Ok(())
            })
        }
    }

    impl hyper::rt::Write for HyperStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_close(cx)
        }
    }

    pub(crate) async fn serve_http<S, F, R, A>(
        addr: A,
        service: S,
        max_connections: usize,
    ) -> io::Result<()>
    where
        S: Fn(Request<hyper::body::Incoming>) -> F + 'static + Copy,
        F: Future<Output = Result<Response<String>, R>> + 'static,
        R: std::error::Error + 'static + Send + Sync,
        A: Into<SocketAddr>,
    {
        let listener = TcpListener::bind(addr.into())?;
        let conn_control = Rc::new(Semaphore::new(max_connections as _));
        loop {
            match listener.accept().await {
                Err(x) => {
                    return Err(x.into());
                }
                Ok(stream) => {
                    let addr = stream.local_addr().unwrap();
                    glommio::spawn_local(enclose!{(conn_control) async move {
                        let _permit = conn_control.acquire_permit(1).await;
                        if let Err(x) = http1::Builder::new().serve_connection(HyperStream(stream), service_fn(service)).await {
                            if !x.is_incomplete_message() {
                                eprintln!("Stream from {addr:?} failed with error {x:?}");
                            }
                        }
                    }}).detach();
                }
            }
        }
    }
}

use glommio::{CpuSet, LocalExecutorPoolBuilder, PoolPlacement};
use hyper::{Method, Request, Response, StatusCode};
use std::convert::Infallible;

async fn hyper_demo(req: Request<hyper::body::Incoming>) -> Result<Response<String>, Infallible> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/hello") => Ok(Response::new("world".to_owned())),
        (&Method::GET, "/world") => Ok(Response::new("hello".to_owned())),
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("notfound".to_owned())
            .unwrap()),
    }
}

fn main() {
    // Issue curl -X GET http://127.0.0.1:8000/hello or curl -X GET http://127.0.0.1:8000/world to
    // see it in action

    println!("Starting server on port 8000");

    LocalExecutorPoolBuilder::new(PoolPlacement::MaxSpread(
        num_cpus::get(),
        CpuSet::online().ok(),
    ))
    .on_all_shards(|| async move {
        let id = glommio::executor().id();
        println!("Starting executor {id}");
        hyper_compat::serve_http(([0, 0, 0, 0], 8000), hyper_demo, 1024)
            .await
            .unwrap();
    })
    .unwrap()
    .join_all();
}
