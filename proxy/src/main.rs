// #![deny(warnings)]

mod extension;

/// Initial implementation taken from [Hyper's own examples][1]. MIT Licensed.
///
/// [1]: https://github.com/hyperium/hyper/blob/684f2fa76d44fa2b1b063ad0443a1b0d16dfad0e/examples/http_proxy.rs
use std::convert::Infallible;
use std::io::ErrorKind;
use std::net::SocketAddr;

use hyper::service::{make_service_fn, service_fn};
use hyper::upgrade::Upgraded;
use hyper::{http, Body, Client, Method, Request, Response, Server};
use prometheus::{register_int_counter, Encoder, IntCounter, TextEncoder};
use tokio::net::TcpStream;

lazy_static::lazy_static! {
  static ref INGRESS_BYTES: IntCounter = register_int_counter!(
    "proxy_ingress_bytes_total",
    "A Counter for the total number of bytes ingested by the HTTP proxy"
  ).unwrap();
  static ref EGRESS_BYTES: IntCounter = register_int_counter!(
    "proxy_egress_bytes_total",
    "A Counter for the total number of bytes sent by the HTTP proxy"
  ).unwrap();
}

type HttpClient = Client<hyper::client::HttpConnector>;

// To try this example:
// 1. cargo run
// 2. config http_proxy in command line
//    $ export http_proxy=http://127.0.0.1:8100
//    $ export https_proxy=http://127.0.0.1:8100
// 3. send requests
//    $ curl -i https://www.some_domain.com/
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 8100));
    let internal_addr = SocketAddr::from(([127, 0, 0, 1], 8101));

    let router = internal_router();
    let service = routerify::RouterService::new(router).unwrap();
    let server = Server::bind(&internal_addr).serve(service);
    println!("Internal server is running on: {}", internal_addr);

    tokio::spawn(async move {
        if let Err(err) = server.await {
            eprintln!("Server error: {}", err);
        }
    });

    let client = Client::builder()
        .http1_title_case_headers(true)
        .http1_preserve_header_case(true)
        .build_http();

    let make_service = make_service_fn(move |_| {
        let client = client.clone();
        async move { Ok::<_, Infallible>(service_fn(move |req| proxy(client.clone(), req))) }
    });

    let server = Server::bind(&addr)
        .http1_preserve_header_case(true)
        .http1_title_case_headers(true)
        .serve(make_service);

    println!("Listening on http://{}", addr);

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}

async fn proxy(client: HttpClient, req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
    println!("req: {:?}", req);

    // TODO(wperron): insert request modification WASM thingy here

    if Method::CONNECT == req.method() {
        // Received an HTTP request like:
        // ```
        // CONNECT www.domain.com:443 HTTP/1.1
        // Host: www.domain.com:443
        // Proxy-Connection: Keep-Alive
        // ```
        //
        // When HTTP method is CONNECT we should return an empty body
        // then we can eventually upgrade the connection and talk a new protocol.
        //
        // Note: only after client received an empty body with STATUS_OK can the
        // connection be upgraded, so we can't return a response inside
        // `on_upgrade` future.
        if let Some(addr) = host_addr(req.uri()) {
            tokio::task::spawn(async move {
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        if let Err(e) = tunnel(upgraded, addr).await {
                            eprintln!("server io error: {}", e);
                        };
                    }
                    Err(e) => eprintln!("upgrade error: {}", e),
                }
            });

            Ok(Response::new(Body::empty()))
        } else {
            eprintln!("CONNECT host is not socket addr: {:?}", req.uri());
            let mut resp = Response::new(Body::from("CONNECT must be to a socket address"));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;

            Ok(resp)
        }
    } else {
        client.request(req).await
    }
}

fn host_addr(uri: &http::Uri) -> Option<String> {
    uri.authority().map(|auth| auth.to_string())
}

// Create a TCP connection to host:port, build a tunnel between the connection and
// the upgraded connection
async fn tunnel(mut upgraded: Upgraded, addr: String) -> std::io::Result<()> {
    // Connect to remote server
    let mut server = TcpStream::connect(addr).await?;

    // Proxying data
    match tokio::io::copy_bidirectional(&mut upgraded, &mut server).await {
        Ok((from_client, from_server)) => {
            // Print message when done
            println!(
                "client wrote {} bytes and received {} bytes",
                from_client, from_server
            );
            EGRESS_BYTES.inc_by(from_client);
            INGRESS_BYTES.inc_by(from_server);
        }
        Err(e) => {
            // not connected error are harmless, so we're simply ignoring them here,
            // returning any other kind of errors.
            if e.kind() != ErrorKind::NotConnected {
                return Err(e);
            }
        }
    }

    Ok(())
}

fn internal_router() -> routerify::Router<Body, Infallible> {
    let builder = routerify::Router::<Body, Infallible>::builder().get("/metrics", metrics);
    builder.build().unwrap()
}

async fn metrics(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let mut buffer = Vec::new();
    let encoder = TextEncoder::new();
    // Gather the metrics.
    let metric_families = prometheus::gather();
    // Encode them to send.
    encoder.encode(&metric_families, &mut buffer).unwrap();
    Ok(Response::new(buffer.into()))
}
