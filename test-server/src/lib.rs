mod data;
use axum::response::{IntoResponse, Response};
use axum::{extract::Path, routing::get, Router};
use data::{META, TARBALLS};
use std::net::SocketAddr;
use std::thread;
pub use thread::JoinHandle;
pub struct TestServer(JoinHandle<()>);

impl TestServer {
    pub async fn spawn_blocking(addr: SocketAddr) {
        let app = Router::new().route("/*all", get(move |p| ret(p, addr)));
        axum::Server::bind(&addr)
            .serve(app.into_make_service())
            .await
            .unwrap();
    }

    pub async fn spawn(addr: SocketAddr) -> TestServer {
        let handle = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    Self::spawn_blocking(addr).await;
                })
        });
        TestServer { 0: handle }
    }
}

async fn ret(Path(params): Path<String>, addr: SocketAddr) -> Response {
    if let Some(meta) = META.get(params.as_str()) {
        meta.replace("{REGISTRY}", &format!("http://{addr}"))
            .into_response()
    } else if let Some(tarball) = TARBALLS.get(params.as_str()) {
        tarball.clone().into_response()
    } else {
        "Not Found".into_response()
    }
}

#[tokio::test]
async fn works() {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    let server = TestServer::spawn(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        8080,
    ))
    .await;
    assert!(
        reqwest::get("http://127.0.0.1:8080/@bendn/test")
            .await
            .unwrap()
            .status()
            != reqwest::StatusCode::NOT_FOUND
    );
    drop(server);
}
