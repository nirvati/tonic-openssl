// Test tonic openssl using example proto

use tonic::{transport::server::TcpConnectInfo, Request, Response, Status};
use tonic_openssl::SslConnectInfo;

tonic::include_proto!("helloworld");

pub struct MyGreeter {}

#[tonic::async_trait]
impl crate::greeter_server::Greeter for MyGreeter {
    async fn say_hello(
        &self,
        request: Request<HelloRequest>,
    ) -> Result<Response<HelloReply>, Status> {
        let remote_addr = request
            .extensions()
            .get::<SslConnectInfo<TcpConnectInfo>>()
            .and_then(|info| info.get_ref().remote_addr());
        println!("Got a request from {:?}", remote_addr);

        let reply = HelloReply {
            message: format!("Hello {}!", request.into_inner().name),
        };
        Ok(Response::new(reply))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        path::{Path, PathBuf},
        time::Duration,
    };

    use openssl::ssl::{
        SslAcceptor, SslAcceptorBuilder, SslConnector, SslConnectorBuilder, SslFiletype, SslMethod,
        SslVerifyMode,
    };
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tokio_util::sync::CancellationToken;
    use tonic::transport::Channel;

    const CERT_DIR: &str = "./tls";
    const TEST_SUBJECT_NAME: &str = "localhost";

    /// Helper function to set connector with alpn protocol and tls version.
    pub fn openssl_configure_connector(
        conn: &mut SslConnectorBuilder,
    ) -> Result<(), tonic_openssl::Error> {
        conn.set_alpn_protos(tonic_openssl::ALPN_H2_WIRE)
            .map_err(tonic_openssl::Error::from)
    }

    pub fn openssl_configure_acceptor(acceptor: &mut SslAcceptorBuilder) {
        acceptor.set_alpn_select_callback(|_ssl, alpn| {
            openssl::ssl::select_next_proto(tonic_openssl::ALPN_H2_WIRE, alpn)
                .ok_or(openssl::ssl::AlpnError::NOACK)
        });
    }
    // get openssl connector with client cert set.
    pub fn get_test_openssl_connector<P: AsRef<Path>>(
        ca_path: P,
        cert_path: P,
        key_path: P,
    ) -> SslConnector {
        let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
        connector.set_ca_file(ca_path.as_ref()).unwrap();
        connector
            .set_certificate_file(cert_path.as_ref(), SslFiletype::PEM)
            .unwrap();
        connector
            .set_private_key_file(key_path.as_ref(), SslFiletype::PEM)
            .unwrap();
        connector.set_verify_callback(
            SslVerifyMode::PEER,
            get_unsafe_verify_callback(TEST_SUBJECT_NAME.to_string()),
        );
        openssl_configure_connector(&mut connector).unwrap();
        connector.build()
    }

    pub async fn connect_test_tonic_channel(
        addr: SocketAddr,
        ca: &Path,
        cert: &Path,
        key: &Path,
    ) -> Result<Channel, tonic::transport::Error> {
        let connector = get_test_openssl_connector(ca, cert, key);
        tonic_openssl::new_endpoint()
            .connect_with_connector(tonic_openssl::connector(
                format!("https://{}", addr).parse().unwrap(),
                connector,
                // test cert has alt subject dns
                Some(TEST_SUBJECT_NAME.to_string()),
            ))
            .await
    }

    // Run the tonic server on the current thread until token is cancelled.
    async fn run_tonic_server(
        token: CancellationToken,
        listener: TcpListener,
        ca: &Path,
        cert: &Path,
        key: &Path,
    ) {
        let greeter = crate::MyGreeter {};
        // build openssl acceptor
        let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        acceptor
            .set_private_key_file(key, SslFiletype::PEM)
            .unwrap();
        acceptor.set_certificate_chain_file(cert).unwrap();
        acceptor.set_ca_file(ca).unwrap();
        acceptor.check_private_key().unwrap();
        openssl_configure_acceptor(&mut acceptor);
        // require client to present cert with matching subject name.
        acceptor.set_verify_callback(
            SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT,
            get_unsafe_verify_callback(TEST_SUBJECT_NAME.to_string()),
        );

        let acceptor = acceptor.build();

        let incoming = tonic_openssl::incoming(TcpListenerStream::new(listener), acceptor);

        tonic::transport::Server::builder()
            .add_service(crate::greeter_server::GreeterServer::new(greeter))
            .serve_with_incoming_shutdown(incoming, async move { token.cancelled().await })
            .await
            .unwrap();
    }

    // creates a listener on a random port from os, and return the addr.
    pub async fn create_listener_server() -> (tokio::net::TcpListener, std::net::SocketAddr) {
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        (listener, local_addr)
    }

    /// TODO: currently the cert in this repo gives this error: unsupported certificate purpose
    /// so we blindly accept all certs for now.
    fn get_unsafe_verify_callback(
        _subject_name: String,
    ) -> impl Fn(bool, &mut openssl::x509::X509StoreContextRef) -> bool {
        move |preverify_ok: bool, ctx: &mut openssl::x509::X509StoreContextRef| {
            if !preverify_ok {
                // cert has problem
                let e = ctx.error();
                println!("verify failed : {}", e);
            }
            true
        }
    }

    /// returns the verify callback that checks the subject name matches in the SN field or Alt field.
    /// This is only an example of how to do validation with self signed certs.
    /// TODO: use this one.
    #[allow(dead_code)]
    fn get_sn_verify_callback(
        subject_name: String,
    ) -> impl Fn(bool, &mut openssl::x509::X509StoreContextRef) -> bool {
        move |preverify_ok: bool, ctx: &mut openssl::x509::X509StoreContextRef| {
            if !preverify_ok {
                // cert has problem
                let e = ctx.error();
                println!("verify failed : {}", e);
                return false;
            }

            // further check subject name.
            let ch = ctx.chain();
            if ch.is_none() {
                return false;
            }
            let ch = ch.unwrap();
            let leaf = ch.iter().last();
            if leaf.is_none() {
                return false;
            }
            let leaf = leaf.unwrap();
            let sn = leaf.subject_name();
            let sn_ok = sn.entries().any(|e| {
                let name = String::from_utf8_lossy(e.data().as_slice());
                println!("sn: {}", name);
                name == subject_name
            });
            if sn_ok {
                return true;
            }
            // sn does not match, so we try to match alt names
            let alt = leaf.subject_alt_names();
            let alt_ok = match alt {
                Some(alts) => alts.into_iter().any(|n| match n.dnsname() {
                    Some(dns) => {
                        println!("alt dns: {}", dns);
                        dns == subject_name
                    }
                    None => false,
                }),
                None => false,
            };
            return alt_ok;
        }
    }

    #[tokio::test]
    async fn basic() {
        let test_ca = PathBuf::from(CERT_DIR).join("ca.pem");
        let test_cert = PathBuf::from(CERT_DIR).join("server.pem");
        let test_key = PathBuf::from(CERT_DIR).join("server.key");

        let test_ca_cp = test_ca.clone();
        let test_cert_cp = test_cert.clone();
        let test_key_cp = test_key.clone();

        // get a random port on localhost from os
        let (listener, addr) = create_listener_server().await;

        let sv_token = CancellationToken::new();
        let sv_token_cp = sv_token.clone();
        // start server in background
        let sv_h = tokio::spawn(async move {
            run_tonic_server(sv_token_cp, listener, &test_ca, &test_cert, &test_key).await
        });

        println!("running server on {addr}");

        // wait a bit for server to boot up.
        tokio::time::sleep(Duration::from_secs(1)).await;

        // TODO: enable this once the failure check is upstreamed.
        // send a request with a wrong cert and verify it fails
        // {
        //     let e = connect_test_tonic_channel(addr, &test_ca, &test_cert, &test_key)
        //         .await
        //         .expect_err("unexpected success");
        //     // there is a double wrappring of the error of ssl Error
        //     let src = e.source().unwrap().source().unwrap();
        //     let ssl_e = src.downcast_ref::<openssl::ssl::Error>().unwrap();
        //     // Check generic ssl error. The detail of the error should be server cert untrusted, which is unimportant,
        //     // since the test case here only aims to cause an ssl failure between client and server.
        //     assert_eq!(ssl_e.code(), openssl::ssl::ErrorCode::SSL);
        //     let inner_e = ssl_e.ssl_error().unwrap().errors();
        //     assert_eq!(inner_e.len(), 1);
        // }

        // get client and send request
        let ch = connect_test_tonic_channel(addr, &test_ca_cp, &test_cert_cp, &test_key_cp)
            .await
            .unwrap();
        let mut client = crate::greeter_client::GreeterClient::new(ch);
        let request = tonic::Request::new(crate::HelloRequest {
            name: "Tonic".into(),
        });
        let resp = client.say_hello(request).await.unwrap();
        println!("RESPONSE={:?}", resp);

        // stop server
        sv_token.cancel();
        sv_h.await.unwrap();
    }
}
