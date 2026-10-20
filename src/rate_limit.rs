use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    net::{IpAddr, SocketAddr},
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    extract::{ConnectInfo, Query, Request, State},
    http::{HeaderMap, header},
    middleware::Next,
    response::Response,
};

use crate::{
    AppState,
    config::Config,
    socket::WsParams,
    token::{self, TokenError},
};

const WINDOW: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(crate) struct RateLimiter {
    ips: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
    users: Mutex<HashMap<String, VecDeque<Instant>>>,
}

fn check<K: Eq + Hash>(
    buckets: &Mutex<HashMap<K, VecDeque<Instant>>>,
    key: K,
    limit: Option<u64>,
) -> Result<(), u64> {
    let Some(limit) = limit else {
        return Ok(());
    };
    if limit == 0 {
        return Err(60);
    }
    let mut buckets = buckets.lock().unwrap_or_else(|error| error.into_inner());
    let now = Instant::now();
    let entries = buckets.entry(key).or_default();
    while entries
        .front()
        .is_some_and(|instant| now.duration_since(*instant) >= WINDOW)
    {
        entries.pop_front();
    }
    if entries.len() as u64 >= limit {
        let remaining = WINDOW
            .saturating_sub(now.duration_since(*entries.front().expect("nonempty rate bucket")));
        return Err(remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0));
    }
    entries.push_back(now);
    Ok(())
}

impl RateLimiter {
    pub fn check_ip(&self, config: &Config, ip: IpAddr) -> Result<(), u64> {
        check(
            &self.ips,
            normalize(ip),
            config.max_requests_per_minute_per_ip,
        )
    }

    pub fn check_user(&self, config: &Config, user: &str) -> Result<(), TokenError> {
        check(
            &self.users,
            user.to_owned(),
            config.max_requests_per_minute_per_user,
        )
        .map_err(TokenError::RateLimited)
    }

    pub fn check_socket(&self, config: &Config, ip: IpAddr, user: &str) -> Result<(), u64> {
        self.check_ip(config, ip)?;
        check(
            &self.users,
            user.to_owned(),
            config.max_requests_per_minute_per_user,
        )
    }

    pub fn prune(&self) {
        let now = Instant::now();
        self.ips
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|_, entries| {
                entries
                    .back()
                    .is_some_and(|last| now.duration_since(*last) < WINDOW)
            });
        self.users
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|_, entries| {
                entries
                    .back()
                    .is_some_and(|last| now.duration_since(*last) < WINDOW)
            });
    }
}

fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}

fn client_ip(config: &Config, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
    let peer = normalize(peer);
    let trusted = |ip| {
        config
            .trusted_proxy_ips
            .iter()
            .any(|proxy| normalize(*proxy) == ip)
    };
    if !trusted(peer) {
        return peer;
    }
    let values = headers.get_all("x-forwarded-for");
    let parsed = values
        .iter()
        .map(|value| value.to_str().ok())
        .collect::<Option<Vec<_>>>()
        .and_then(|values| {
            values
                .into_iter()
                .flat_map(|value| value.split(','))
                .map(|value| value.trim().parse::<IpAddr>().ok().map(normalize))
                .collect::<Option<Vec<_>>>()
        });
    let Some(chain) = parsed else {
        return peer;
    };
    let mut ip = peer;
    for previous in chain.into_iter().rev() {
        if !trusted(ip) {
            break;
        }
        ip = previous;
    }
    ip
}

pub(crate) async fn middleware(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    let ip = client_ip(&state.config, peer.ip(), request.headers());
    request.extensions_mut().insert(ClientIp(ip));
    if let Err(seconds) = state.limits.check_ip(&state.config, ip) {
        return response(seconds);
    }
    if state.config.max_requests_per_minute_per_user.is_some()
        && !matches!(request.uri().path(), "/token" | "/identity/import")
    {
        let bearer = if request.uri().path() == "/ws" {
            Query::<WsParams>::try_from_uri(request.uri())
                .ok()
                .map(|Query(params)| params.token)
        } else {
            request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split_once(' '))
                .filter(|(scheme, value)| {
                    scheme.eq_ignore_ascii_case("bearer") && value.len() == 37
                })
                .map(|(_, value)| value.to_owned())
        };
        if let Some(bearer) = bearer {
            let auth_state = state.clone();
            match tokio::task::spawn_blocking(move || {
                token::authenticate_user(&auth_state, &bearer)
            })
            .await
            {
                Ok(Ok(user)) => {
                    if let Err(error) = state.limits.check_user(&state.config, &user.user_id) {
                        return axum::response::IntoResponse::into_response(error);
                    }
                }
                Ok(Err(TokenError::Unauthorized(_))) => {}
                Ok(Err(error)) => return axum::response::IntoResponse::into_response(error),
                Err(error) => {
                    return axum::response::IntoResponse::into_response(crate::identity::internal(
                        error,
                    ));
                }
            }
        }
    }
    next.run(request).await
}

#[derive(Clone, Copy)]
pub(crate) struct ClientIp(pub IpAddr);

pub(crate) fn response(seconds: u64) -> Response {
    let mut response = crate::status(
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        "request rate limit exceeded",
    );
    response.headers_mut().insert(
        header::RETRY_AFTER,
        seconds
            .max(1)
            .to_string()
            .parse()
            .expect("numeric Retry-After"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}
