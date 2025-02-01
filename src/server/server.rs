use {
    super::{
        deserialize_from_str::DeserializeFromStr,
        error::{OptionExt, ServerError, ServerResult},
        ServerConfig,
    },
    crate::{
        api::{self, content::AcceptEncoding},
        index::{Index, RpcClientProvider},
        models::{InscriptionId, Pagination, Subscription},
        subscription::SubscriptionManager,
    },
    axum::{
        extract::{DefaultBodyLimit, Extension, FromRef, Json, Path, Query},
        response::IntoResponse,
        routing::{get, post},
        Router,
    },
    axum_server::Handle,
    bitcoin::{address::NetworkUnchecked, Address, OutPoint, Txid},
    http::StatusCode,
    std::{io, net::ToSocketAddrs, sync::Arc},
    tokio::task,
    tower_http::{
        compression::CompressionLayer,
        cors::{Any, CorsLayer},
    },
    tracing::{error, info},
    uuid::Uuid,
};

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("failed to bind to address")]
    BindError(#[from] std::io::Error),
    #[error("failed to parse address")]
    SocketAddrError(#[from] std::net::AddrParseError),
    #[error("no socket address found")]
    NoSocketAddr,
}

type SpawnResult<T> = std::result::Result<T, SpawnError>;

pub struct Server;

impl Server {
    pub fn start(
        &self,
        index: Arc<Index>,
        subscription_manager: Arc<SubscriptionManager>,
        config: Arc<ServerConfig>,
        handle: Handle,
    ) -> SpawnResult<task::JoinHandle<io::Result<()>>> {
        let router = Router::new()
            // Status
            .route("/status", get(Self::status))
            // Blocks
            .route("/tip", get(Self::tip))
            .route("/block/{query}", get(Self::block))
            // Addresses
            .route("/address/{address}", get(Self::address))
            // Transactions
            .route("/tx/{txid}", get(Self::transaction))
            .route("/output/{outpoint}", get(Self::output))
            // Inscriptions
            .route("/inscription/{inscription_id}", get(Self::inscription))
            // Runes
            .route("/runes", get(Self::runes))
            .route("/rune/{rune}", get(Self::rune))
            .route("/rune/{rune}/transactions", get(Self::rune_transactions))
            // Mempool
            .route("/mempool/txids", get(Self::mempool_txids))
            // Subscriptions
            .route("/subscription/{id}", get(Self::get_subscription))
            .route(
                "/subscription",
                post(Self::add_subscription).delete(Self::delete_subscription),
            )
            .route("/subscriptions", get(Self::subscriptions))
            .layer(Extension(index))
            .layer(Extension(subscription_manager))
            .layer(Extension(config.clone()))
            .layer(
                CorsLayer::new()
                    .allow_methods([http::Method::GET])
                    .allow_origin(Any),
            )
            .layer(DefaultBodyLimit::disable())
            .layer(CompressionLayer::new())
            .with_state(config.clone());

        let jh = self.spawn(&config, router, handle)?;

        Ok(jh)
    }

    fn spawn(
        &self,
        config: &ServerConfig,
        router: Router,
        handle: Handle,
    ) -> SpawnResult<task::JoinHandle<io::Result<()>>> {
        let addr = config
            .http_listen
            .to_socket_addrs()?
            .next()
            .ok_or(SpawnError::NoSocketAddr)?;

        info!("Listening on http://{addr}");

        Ok(tokio::spawn(async move {
            axum_server::Server::bind(addr)
                .handle(handle)
                .serve(router.into_make_service())
                .await
        }))
    }

    async fn tip(Extension(index): Extension<Arc<Index>>) -> ServerResult {
        task::block_in_place(|| Ok(Json(api::tip(index)?).into_response()))
    }

    async fn status(Extension(index): Extension<Arc<Index>>) -> ServerResult {
        task::block_in_place(|| Ok(Json(api::status(index)?).into_response()))
    }

    async fn block(
        Extension(index): Extension<Arc<Index>>,
        Path(DeserializeFromStr(query)): Path<DeserializeFromStr<api::query::Block>>,
    ) -> ServerResult {
        task::block_in_place(|| Ok(Json(api::block(index, &query)?).into_response()))
    }

    async fn transaction(
        Extension(index): Extension<Arc<Index>>,
        Extension(config): Extension<Arc<ServerConfig>>,
        Path(txid): Path<Txid>,
    ) -> ServerResult {
        task::block_in_place(|| {
            Ok(Json(api::transaction(
                index,
                config.get_new_rpc_client()?,
                &txid,
            )?)
            .into_response())
        })
    }

    async fn output(
        Extension(index): Extension<Arc<Index>>,
        Path(outpoint): Path<OutPoint>,
    ) -> ServerResult {
        task::block_in_place(|| Ok(Json(api::output(index, &outpoint)?).into_response()))
    }

    async fn runes(
        Extension(index): Extension<Arc<Index>>,
        Query(pagination): Query<Pagination>,
    ) -> ServerResult {
        task::block_in_place(|| Ok(Json(api::runes(index, pagination)?).into_response()))
    }

    async fn rune(
        Extension(index): Extension<Arc<Index>>,
        Path(DeserializeFromStr(rune)): Path<DeserializeFromStr<api::query::Rune>>,
    ) -> ServerResult {
        task::block_in_place(|| Ok(Json(api::rune(index, &rune)?).into_response()))
    }

    async fn rune_transactions(
        Extension(index): Extension<Arc<Index>>,
        Path(DeserializeFromStr(rune)): Path<DeserializeFromStr<api::query::Rune>>,
        Query(pagination): Query<Pagination>,
    ) -> ServerResult {
        task::block_in_place(|| {
            Ok(Json(api::last_rune_transactions(index, &rune, Some(pagination))?).into_response())
        })
    }

    async fn inscription(
        Extension(index): Extension<Arc<Index>>,
        Extension(config): Extension<Arc<ServerConfig>>,
        Path(DeserializeFromStr(inscription_id)): Path<DeserializeFromStr<InscriptionId>>,
        accept_encoding: AcceptEncoding,
    ) -> ServerResult {
        task::block_in_place(|| {
            Ok(api::inscription_content(
                index,
                &inscription_id,
                accept_encoding,
                config.csp_origin.clone(),
                config.decompress,
            )?
            .ok_or_not_found(|| format!("inscription {inscription_id} content"))?
            .into_response())
        })
    }

    async fn mempool_txids(Extension(index): Extension<Arc<Index>>) -> ServerResult {
        task::block_in_place(|| Ok(Json(api::mempool_txids(index)?).into_response()))
    }

    async fn address(
        Extension(index): Extension<Arc<Index>>,
        Extension(config): Extension<Arc<ServerConfig>>,
        Path(address): Path<Address<NetworkUnchecked>>,
    ) -> ServerResult {
        let address = address
            .require_network(config.chain.network())
            .map_err(|err| ServerError::BadRequest(err.to_string()))?;

        task::block_in_place(|| Ok(Json(api::address(index, &address)?).into_response()))
    }

    async fn subscriptions(
        Extension(subscription_manager): Extension<Arc<SubscriptionManager>>,
        Extension(config): Extension<Arc<ServerConfig>>,
    ) -> ServerResult {
        if !config.enable_subscriptions {
            return Err(ServerError::BadRequest(
                "subscriptions are not enabled".to_string(),
            ));
        }

        task::block_in_place(|| Ok(Json(api::subscriptions(subscription_manager)?).into_response()))
    }

    async fn add_subscription(
        Extension(subscription_manager): Extension<Arc<SubscriptionManager>>,
        Extension(config): Extension<Arc<ServerConfig>>,
        Json(subscription): Json<Subscription>,
    ) -> ServerResult {
        if !config.enable_subscriptions {
            return Err(ServerError::BadRequest(
                "subscriptions are not enabled".to_string(),
            ));
        }

        task::block_in_place(|| {
            Ok(Json(api::add_subscription(subscription_manager, subscription)?).into_response())
        })
    }

    async fn delete_subscription(
        Extension(subscription_manager): Extension<Arc<SubscriptionManager>>,
        Extension(config): Extension<Arc<ServerConfig>>,
        Path(id): Path<Uuid>,
    ) -> ServerResult {
        if !config.enable_subscriptions {
            return Err(ServerError::BadRequest(
                "subscriptions are not enabled".to_string(),
            ));
        }

        task::block_in_place(|| {
            Ok(Json(api::delete_subscription(subscription_manager, id)?).into_response())
        })
    }

    async fn get_subscription(
        Extension(subscription_manager): Extension<Arc<SubscriptionManager>>,
        Extension(config): Extension<Arc<ServerConfig>>,
        Path(id): Path<Uuid>,
    ) -> ServerResult {
        if !config.enable_subscriptions {
            return Err(ServerError::BadRequest(
                "subscriptions are not enabled".to_string(),
            ));
        }

        task::block_in_place(|| {
            Ok(Json(api::get_subscription(subscription_manager, id)?).into_response())
        })
    }
}

impl<S> axum::extract::FromRequestParts<S> for AcceptEncoding
where
    Arc<ServerConfig>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.headers.get("accept-encoding").map(|value| {
            value.to_str().unwrap_or_default().to_owned()
        })))
    }
}
