// Copyright (c) Microsoft Corporation
// SPDX-License-Identifier: MIT

//! This module is responsible for starting the proxy server and handling incoming requests.
//! It listens on a specified port and forwards the requests to the target server,
//!  then forward the response from the target server and sends it back to the client.
//! It also handles the provision state check request.
//! It uses the `hyper` crate to handle the HTTP requests and responses,
//!  uses the `tower` crate to limit the incoming request body size.
//!
//! Example:
//! ```rust
//! use crate::common::config;
//! use crate::proxy::proxy_server;
//! use crate::shared_state::SharedState;
//!
//! let shared_state = Arc::new(Mutex::new(SharedState::new()));
//! let port = config::get_proxy_port();
//! tokio::spawn(proxy_server::start(port, shared_state.clone()));
//! ```

use crate::common::{
    config, constants, error::Error, helpers, hyper_client, logger, result::Result,
};
use crate::proxy::proxy_connection::{Connection, ConnectionContext};
use crate::proxy::{proxy_authorizer, proxy_summary::ProxySummary, Claims};
use crate::shared_state::{
    agent_status_wrapper, key_keeper_wrapper, proxy_listener_wrapper, tokio_wrapper, SharedState,
};
use crate::{provision, redirector};
use http_body_util::Full;
use http_body_util::{combinators::BoxBody, BodyExt};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::StatusCode;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use proxy_agent_shared::misc_helpers;
use proxy_agent_shared::proxy_agent_aggregate_status::ModuleState;
use proxy_agent_shared::proxy_agent_aggregate_status::ProxyAgentDetailStatus;
use proxy_agent_shared::telemetry::event_logger;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tower::Service;
use tower_http::{body::Limited, limit::RequestBodyLimitLayer};

const INITIAL_CONNECTION_ID: u128 = 0;
const REQUEST_BODY_LOW_LIMIT_SIZE: usize = 1024 * 100; // 100KB
const REQUEST_BODY_LARGE_LIMIT_SIZE: usize = 1024 * REQUEST_BODY_LOW_LIMIT_SIZE; // 100MB
const START_LISTENER_RETRY_COUNT: u16 = 5;
const START_LISTENER_RETRY_SLEEP_DURATION: Duration = Duration::from_secs(1);

pub fn stop(shared_state: Arc<Mutex<SharedState>>) {
    proxy_listener_wrapper::set_shutdown(shared_state.clone(), true);
}

pub fn get_status(shared_state: Arc<Mutex<SharedState>>) -> ProxyAgentDetailStatus {
    let status = if proxy_listener_wrapper::get_shutdown(shared_state.clone()) {
        ModuleState::STOPPED
    } else {
        ModuleState::RUNNING
    };

    ProxyAgentDetailStatus {
        status,
        message: proxy_listener_wrapper::get_status_message(shared_state.clone()),
        states: None,
    }
}

/// start listener at the given address with retry logic if the address is in use
async fn start_listener_with_retry(
    addr: &str,
    retry_count: u16,
    sleep_duration: Duration,
) -> Result<TcpListener> {
    for i in 0..retry_count {
        let listener = TcpListener::bind(addr).await;
        match listener {
            Ok(l) => {
                return Ok(l);
            }
            Err(e) => match e.kind() {
                std::io::ErrorKind::AddrInUse => {
                    let message =
                        format!(
                        "Failed bind to '{}' with error 'AddrInUse', wait '{:#?}' and retrying {}.",
                        addr, sleep_duration, (i+1)
                    );
                    logger::write_warning(message);
                    tokio::time::sleep(sleep_duration).await;
                    continue;
                }
                _ => {
                    // other error, return it
                    return Err(Error::Io(
                        format!("Failed to bind TcpListener '{}'", addr),
                        e,
                    ));
                }
            },
        }
    }

    // one more effort try bind to the addr
    TcpListener::bind(addr)
        .await
        .map_err(|e| Error::Io(format!("Failed to bind TcpListener '{}'", addr), e))
}

pub async fn start(port: u16, shared_state: Arc<Mutex<SharedState>>) {
    Connection::init_logger(config::get_logs_dir());

    let addr = format!("{}:{}", std::net::Ipv4Addr::LOCALHOST, port);
    logger::write(format!("Start proxy listener at '{}'.", &addr));

    let listener = match start_listener_with_retry(
        &addr,
        START_LISTENER_RETRY_COUNT,
        START_LISTENER_RETRY_SLEEP_DURATION,
    )
    .await
    {
        Ok(listener) => listener,
        Err(e) => {
            let message = e.to_string();
            proxy_listener_wrapper::set_status_message(shared_state.clone(), message.to_string());
            // send this critical error to event logger
            event_logger::write_event(
                event_logger::WARN_LEVEL,
                message,
                "start",
                "proxy_server",
                Connection::CONNECTION_LOGGER_KEY,
            );

            return;
        }
    };

    let message = helpers::write_startup_event(
        "Started proxy listener, ready to accept request",
        "start",
        "proxy_server",
        logger::AGENT_LOGGER_KEY,
    );
    proxy_listener_wrapper::set_status_message(shared_state.clone(), message.to_string());
    provision::listener_started(shared_state.clone());

    let cancellation_token = tokio_wrapper::get_cancellation_token(shared_state.clone());
    // We start a loop to continuously accept incoming connections
    loop {
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                logger::write_warning("cancellation token signal received, stop the listener.".to_string());
                return;
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, client_addr)) =>{
                        accept_one_request(stream, client_addr, shared_state.clone()).await;
                    },
                    Err(e) => {
                        logger::write_error(format!("Failed to accept connection: {}", e));
                    }
                }
            }
        }
    }
}

async fn accept_one_request(
    stream: TcpStream,
    client_addr: std::net::SocketAddr,
    shared_state: Arc<Mutex<SharedState>>,
) {
    Connection::write(
        INITIAL_CONNECTION_ID,
        "Accepted new connection.".to_string(),
    );
    let shared_state = shared_state.clone();
    tokio::spawn(async move {
        let (stream, cloned_std_stream) = match set_stream_read_time_out(stream) {
            Ok((stream, cloned_std_stream)) => (stream, cloned_std_stream),
            Err(e) => {
                Connection::write_error(
                    INITIAL_CONNECTION_ID,
                    format!("Failed to set stream read timeout: {}", e),
                );
                return;
            }
        };
        let cloned_std_stream = Arc::new(Mutex::new(cloned_std_stream));
        // move client addr, cloned std stream and shared_state to the service_fn
        let service = service_fn(move |req| {
            // use tower service as middleware to limit the request body size
            let low_limit_layer = RequestBodyLimitLayer::new(REQUEST_BODY_LOW_LIMIT_SIZE);
            let large_limit_layer = RequestBodyLimitLayer::new(REQUEST_BODY_LARGE_LIMIT_SIZE);
            let low_limited_tower_service = tower::ServiceBuilder::new().layer(low_limit_layer);
            let large_limited_tower_service = tower::ServiceBuilder::new().layer(large_limit_layer);
            let tower_service_layer =
                if crate::common::hyper_client::should_skip_sig(req.method(), req.uri()) {
                    // skip signature check for large request
                    large_limited_tower_service.clone()
                } else {
                    // use low limit for normal request
                    low_limited_tower_service.clone()
                };

            let shared_state = shared_state.clone();
            let cloned_std_stream = cloned_std_stream.clone();
            let mut tower_service = tower_service_layer.service_fn(move |req: Request<_>| {
                let connection = ConnectionContext {
                    stream: cloned_std_stream.clone(),
                    client_addr,
                    id: INITIAL_CONNECTION_ID,
                    now: std::time::Instant::now(),
                    method: req.method().clone(),
                    url: req.uri().clone(),
                    ip: None,
                    port: 0,
                    claims: None,
                };

                handle_request(req, connection, shared_state.clone())
            });
            tower_service.call(req)
        });

        // Use an adapter to access something implementing `tokio::io` traits as if they implement
        let io = TokioIo::new(stream);
        // We use the `hyper::server::conn::Http` to serve the connection
        let http = hyper::server::conn::http1::Builder::new();
        if let Err(e) = http.serve_connection(io, service).await {
            Connection::write_warning(
                INITIAL_CONNECTION_ID,
                format!("ProxyListener serve_connection error: {}", e),
            );
        }
    });
}

// Set the read timeout for the stream
fn set_stream_read_time_out(stream: TcpStream) -> Result<(TcpStream, std::net::TcpStream)> {
    // Convert the stream to a std stream
    let std_stream = stream.into_std().map_err(|e| {
        Error::Io(
            "Failed to convert Tokio stream into std equivalent".to_string(),
            e,
        )
    })?;

    // Set the read timeout
    if let Err(e) = std_stream.set_read_timeout(Some(std::time::Duration::from_secs(10))) {
        Connection::write_warning(
            INITIAL_CONNECTION_ID,
            format!("Failed to set read timeout: {}", e),
        );
    }

    // Clone the stream for the service_fn
    let cloned_std_stream = std_stream
        .try_clone()
        .map_err(|e| Error::Io("Failed to clone TCP stream".to_string(), e))?;

    // Convert the std stream back
    let tokio_tcp_stream = TcpStream::from_std(std_stream).map_err(|e| {
        Error::Io(
            "Failed to convert std stream into Tokio equivalent".to_string(),
            e,
        )
    })?;

    Ok((tokio_tcp_stream, cloned_std_stream))
}

async fn handle_request(
    request: Request<Limited<hyper::body::Incoming>>,
    mut connection: ConnectionContext,
    shared_state: Arc<Mutex<SharedState>>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
    let connection_id = proxy_listener_wrapper::increase_connection_count(shared_state.clone());
    connection.id = connection_id;
    Connection::write_information(
        connection_id,
        format!(
            "Got request from {} for {} {}",
            connection.client_addr, connection.method, connection.url
        ),
    );

    if connection.url == provision::PROVISION_URL_PATH {
        return handle_provision_state_check_request(connection.id, request, shared_state.clone())
            .await;
    }

    let client_source_ip = connection.client_addr.ip();
    let client_source_port = connection.client_addr.port();

    let mut entry = None;
    match redirector::lookup_audit(client_source_port, shared_state.clone()) {
        Ok(data) => entry = Some(data),
        Err(e) => {
            let err = format!("Failed to get lookup_audit: {}", e);
            event_logger::write_event(
                event_logger::WARN_LEVEL,
                err,
                "handle_request",
                "proxy_server",
                Connection::CONNECTION_LOGGER_KEY,
            );
            #[cfg(windows)]
            {
                Connection::write_information(
                    connection_id,
                    "Try to get audit entry from socket stream".to_string(),
                );
                use std::os::windows::io::AsRawSocket;
                match redirector::get_audit_from_stream_socket(
                    connection.stream.lock().unwrap().as_raw_socket() as usize,
                ) {
                    Ok(data) => entry = Some(data),
                    Err(e) => {
                        let err = format!("Failed to get lookup_audit_from_stream: {}", e);
                        event_logger::write_event(
                            event_logger::WARN_LEVEL,
                            err,
                            "handle_request",
                            "proxy_server",
                            Connection::CONNECTION_LOGGER_KEY,
                        );
                    }
                }
            }
        }
    }
    let entry = match entry {
        Some(e) => e,
        None => {
            log_connection_summary(
                &connection,
                StatusCode::MISDIRECTED_REQUEST,
                shared_state.clone(),
            );
            return Ok(empty_response(StatusCode::MISDIRECTED_REQUEST));
        }
    };

    let claims = match Claims::from_audit_entry(&entry, client_source_ip, shared_state.clone()) {
        Ok(claims) => claims,
        Err(e) => {
            Connection::write_warning(
                connection_id,
                format!("Failed to get claims from audit entry: {}", e),
            );
            log_connection_summary(
                &connection,
                StatusCode::MISDIRECTED_REQUEST,
                shared_state.clone(),
            );
            return Ok(empty_response(StatusCode::MISDIRECTED_REQUEST));
        }
    };

    let claim_details: String = match serde_json::to_string(&claims) {
        Ok(json) => json,
        Err(e) => {
            Connection::write_warning(
                connection_id,
                format!("Failed to get claim json string: {}", e),
            );
            log_connection_summary(
                &connection,
                StatusCode::MISDIRECTED_REQUEST,
                shared_state.clone(),
            );
            return Ok(empty_response(StatusCode::MISDIRECTED_REQUEST));
        }
    };
    Connection::write(connection_id, claim_details.to_string());
    connection.claims = Some(claims.clone());

    // Get the dst ip and port to remote server
    let (ip, port);
    ip = entry.destination_ipv4_addr();
    port = entry.destination_port_in_host_byte_order();
    Connection::write(connection_id, format!("Use lookup value:{ip}:{port}."));
    connection.ip = Some(ip);
    connection.port = port;

    // authenticate the connection
    if !proxy_authorizer::authorize(
        ip.to_string(),
        port,
        connection_id,
        request.uri().clone(),
        claims.clone(),
        shared_state.clone(),
    ) {
        Connection::write_warning(
            connection_id,
            format!("Denied unauthorize request: {}", claim_details),
        );
        log_connection_summary(&connection, StatusCode::FORBIDDEN, shared_state.clone());
        return Ok(empty_response(StatusCode::FORBIDDEN));
    }

    // forward the request to the target server
    let mut proxy_request = request;

    // Add required headers
    let host_claims = format!(
        "{{ \"{}\": \"{}\"}}",
        constants::CLAIMS_IS_ROOT,
        claims.runAsElevated
    );
    proxy_request.headers_mut().insert(
        HeaderName::from_static(constants::CLAIMS_HEADER),
        match HeaderValue::from_str(&host_claims) {
            Ok(value) => value,
            Err(e) => {
                Connection::write_error(
                    connection_id,
                    format!(
                        "Failed to add claims header: {} with error: {}",
                        host_claims, e
                    ),
                );
                return Ok(empty_response(StatusCode::BAD_GATEWAY));
            }
        },
    );
    proxy_request.headers_mut().insert(
        HeaderName::from_static(constants::DATE_HEADER),
        match HeaderValue::from_str(&misc_helpers::get_date_time_rfc1123_string()) {
            Ok(value) => value,
            Err(e) => {
                Connection::write_error(
                    connection_id,
                    format!("Failed to add date header with error: {}", e),
                );
                return Ok(empty_response(StatusCode::BAD_GATEWAY));
            }
        },
    );

    if connection.should_skip_sig() {
        Connection::write(
            connection_id,
            format!(
                "Skip compute signature for the request for {} {}",
                connection.method, connection.url
            ),
        );
    } else {
        return handle_request_with_signature(connection, proxy_request, shared_state).await;
    }

    // start new request to the Host endpoint
    let proxy_response =
        hyper_client::send_request(ip.to_string().as_str(), port, proxy_request, move |msg| {
            Connection::write_warning(connection_id, msg);
        })
        .await;
    forward_response(proxy_response, connection, shared_state).await
}

async fn handle_provision_state_check_request(
    connection_id: u128,
    request: Request<Limited<hyper::body::Incoming>>,
    shared_state: Arc<Mutex<SharedState>>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
    // check MetaData header exists or not
    if request.headers().get(constants::METADATA_HEADER).is_none() {
        Connection::write_warning(
            connection_id,
            "No MetaData header found in the request.".to_string(),
        );
        return Ok(empty_response(StatusCode::BAD_REQUEST));
    }

    // notify key_keeper to poll the status
    key_keeper_wrapper::notify(shared_state.clone());

    let provision_state = provision::get_provision_state(shared_state);
    match serde_json::to_string(&provision_state) {
        Ok(json) => {
            Connection::write(connection_id, format!("Provision state: {}", json));
            let mut response = Response::new(hyper_client::full_body(json.as_bytes().to_vec()));
            response.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            );
            Ok(response)
        }
        Err(e) => {
            let error = format!("Failed to get provision state: {}", e);
            Connection::write_warning(connection_id, error.to_string());
            let mut response = Response::new(hyper_client::full_body(error.as_bytes().to_vec()));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            Ok(response)
        }
    }
}

async fn forward_response(
    proxy_response: Result<Response<Incoming>>,
    connection: ConnectionContext,
    shared_state: Arc<Mutex<SharedState>>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
    let connection_id = connection.id;
    let proxy_response = match proxy_response {
        Ok(response) => response,
        Err(e) => {
            Connection::write_warning(
                connection_id,
                format!("Failed to send request to host: {}", e),
            );
            log_connection_summary(
                &connection,
                StatusCode::SERVICE_UNAVAILABLE,
                shared_state.clone(),
            );
            return Ok(empty_response(StatusCode::SERVICE_UNAVAILABLE));
        }
    };

    let (head, body) = proxy_response.into_parts();
    let frame_stream = body.map_frame(move |frame| {
        let frame = match frame.into_data() {
            Ok(data) => data.iter().map(|byte| byte.to_be()).collect::<Bytes>(),
            Err(e) => {
                Connection::write_error(
                    connection_id,
                    format!("Failed to get frame data: {:?}", e),
                );
                Bytes::new()
            }
        };

        Frame::data(frame)
    });
    let mut response = Response::from_parts(head, frame_stream.boxed());

    // insert default x-ms-azure-host-authorization header to let the client know it is through proxy agent
    response.headers_mut().insert(
        HeaderName::from_static(constants::AUTHORIZATION_HEADER),
        HeaderValue::from_static("value"),
    );

    log_connection_summary(&connection, response.status(), shared_state.clone());
    Ok(response)
}

fn log_connection_summary(
    connection: &ConnectionContext,
    response_status: StatusCode,
    shared_state: Arc<Mutex<SharedState>>,
) {
    let elapsed_time = connection.now.elapsed();
    let claims = match &connection.claims {
        Some(c) => c.clone(),
        None => Claims::empty(),
    };

    let summary = ProxySummary {
        id: connection.id,
        userId: claims.userId,
        userName: claims.userName.to_string(),
        userGroups: claims.userGroups.clone(),
        clientIp: claims.clientIp.to_string(),
        processFullPath: claims.processFullPath.to_string(),
        processCmdLine: claims.processCmdLine.to_string(),
        runAsElevated: claims.runAsElevated,
        method: connection.method.to_string(),
        url: connection.url.to_string(),
        ip: connection.get_ip_string(),
        port: connection.port,
        responseStatus: response_status.to_string(),
        elapsedTime: elapsed_time.as_millis(),
    };
    if let Ok(json) = serde_json::to_string(&summary) {
        event_logger::write_event(
            event_logger::INFO_LEVEL,
            json,
            "log_connection_summary",
            "proxy_server",
            Connection::CONNECTION_LOGGER_KEY,
        );
    };
    agent_status_wrapper::add_one_connection_summary(shared_state, summary, false);
}

// We create some utility functions to make Empty and Full bodies
// fit our broadened Response body type.
fn empty_response(status_code: StatusCode) -> Response<BoxBody<Bytes, hyper::Error>> {
    let mut response = Response::new(hyper_client::empty_body());
    *response.status_mut() = status_code;

    response
}

async fn handle_request_with_signature(
    connection: ConnectionContext,
    request: Request<Limited<Incoming>>,
    shared_state: Arc<Mutex<SharedState>>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
    let (head, body) = request.into_parts();
    let whole_body = match body.collect().await {
        Ok(data) => data.to_bytes(),
        Err(e) => {
            Connection::write_error(
                connection.id,
                format!("Failed to receive the request body: {}", e),
            );
            return Ok(empty_response(StatusCode::BAD_REQUEST));
        }
    };

    Connection::write(
        connection.id,
        format!(
            "Received the client request body (len={}) for {} {}",
            whole_body.len(),
            connection.method,
            connection.url,
        ),
    );

    // create a new request to the Host endpoint
    let mut proxy_request: Request<Full<Bytes>> =
        Request::from_parts(head.clone(), Full::new(whole_body.clone()));

    // sign the request
    // Add header x-ms-azure-host-authorization
    if let (Some(key), Some(key_guid)) = (
        key_keeper_wrapper::get_current_key_value(shared_state.clone()),
        key_keeper_wrapper::get_current_key_guid(shared_state.clone()),
    ) {
        let input_to_sign = hyper_client::as_sig_input(head, whole_body);
        match helpers::compute_signature(&key, input_to_sign.as_slice()) {
            Ok(sig) => {
                let authorization_value =
                    format!("{} {} {}", constants::AUTHORIZATION_SCHEME, key_guid, sig);
                proxy_request.headers_mut().insert(
                    HeaderName::from_static(constants::AUTHORIZATION_HEADER),
                    match HeaderValue::from_str(&authorization_value) {
                        Ok(value) => value,
                        Err(e) => {
                            Connection::write_error(
                                connection.id,
                                format!(
                                    "Failed to add authorization header: {} with error: {}",
                                    authorization_value, e
                                ),
                            );
                            return Ok(empty_response(StatusCode::BAD_GATEWAY));
                        }
                    },
                );

                Connection::write(
                    connection.id,
                    format!("Added authorization header {}", authorization_value),
                )
            }
            Err(e) => {
                Connection::write_error(
                    connection.id,
                    format!("compute_signature failed with error: {}", e),
                );
            }
        }
    } else {
        Connection::write(
            connection.id,
            "current key is empty, skip computing the signature.".to_string(),
        );
    }

    // start new request to the Host endpoint
    let connection_id = connection.id;
    let proxy_response = hyper_client::send_request(
        &connection.get_ip_string(),
        connection.port,
        proxy_request,
        move |msg| {
            Connection::write_warning(connection_id, msg);
        },
    )
    .await;

    forward_response(proxy_response, connection, shared_state).await
}

#[cfg(test)]
mod tests {
    use crate::common::hyper_client;
    use crate::common::logger;
    use crate::proxy::proxy_connection::Connection;
    use crate::proxy::proxy_server;
    use crate::shared_state::key_keeper_wrapper;
    use crate::shared_state::SharedState;
    use http::Method;
    use proxy_agent_shared::logger_manager;
    use std::collections::HashMap;
    use std::env;
    use std::fs;
    use std::time::Duration;

    #[tokio::test]
    async fn direct_request_test() {
        let logger_key = "direct_request_test";
        let mut temp_test_path = env::temp_dir();
        temp_test_path.push(logger_key);
        logger_manager::init_logger(
            logger::AGENT_LOGGER_KEY.to_string(), // production code uses 'Agent_Log' to write.
            temp_test_path.clone(),
            logger_key.to_string(),
            10 * 1024 * 1024,
            20,
        );
        Connection::init_logger(temp_test_path.to_path_buf());

        // start listener, the port must different from the one used in production code
        let shared_state = SharedState::new();
        let s = shared_state.clone();
        let host = "127.0.0.1";
        let port: u16 = 8091;
        tokio::spawn(proxy_server::start(port, s.clone()));

        // give some time to let the listener started
        let sleep_duration = Duration::from_millis(100);
        tokio::time::sleep(sleep_duration).await;

        let url: hyper::Uri = format!("http://{}:{}/", host, port).parse().unwrap();
        let request = hyper_client::build_request(
            Method::GET,
            &url,
            &HashMap::new(),
            None,
            key_keeper_wrapper::get_current_key_guid(shared_state.clone()),
            key_keeper_wrapper::get_current_key_value(shared_state.clone()),
        )
        .unwrap();
        let response = hyper_client::send_request(host, port, request, logger::write_warning)
            .await
            .unwrap();
        assert_eq!(
            http::StatusCode::MISDIRECTED_REQUEST,
            response.status(),
            "response.status must be MISDIRECTED_REQUEST."
        );

        // test large request body
        let body = vec![88u8; super::REQUEST_BODY_LOW_LIMIT_SIZE + 1];
        let request = hyper_client::build_request(
            Method::POST,
            &url,
            &HashMap::new(),
            Some(body.as_slice()),
            key_keeper_wrapper::get_current_key_guid(shared_state.clone()),
            key_keeper_wrapper::get_current_key_value(shared_state.clone()),
        )
        .unwrap();
        let response = hyper_client::send_request(host, port, request, logger::write_warning)
            .await
            .unwrap();
        assert_eq!(
            http::StatusCode::PAYLOAD_TOO_LARGE,
            response.status(),
            "response.status must be PAYLOAD_TOO_LARGE."
        );

        // stop listener
        proxy_server::stop(shared_state);

        // clean up and ignore the clean up errors
        _ = fs::remove_dir_all(temp_test_path);
    }
}
