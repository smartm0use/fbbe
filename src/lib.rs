pub use crate::error::Error;
use crate::globals::{init_globals, network};
use crate::route::route_infallible;
use crate::state::SharedState;
use crate::threads::bootstrap_state::bootstrap_state_infallible;
use crate::threads::update_chain_info::update_chain_info_infallible;
use crate::threads::update_mempool_info::update_mempool;
use bitcoin::Network;
use clap::Parser;
use globals::networks;
use hyper::service::{make_service_fn, service_fn};
use hyper::Server;
use network_parse::NetworkParse;
use std::convert::Infallible;
use std::fmt::Display;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::time::sleep;

mod base_text_decorator;
mod error;
mod globals;
mod network_parse;
mod pages;
mod render;
mod req;
mod route;
mod rpc;
mod state;
mod threads;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Arguments {
    /// Number of transaction kept in memory in a least recently used cache to reduce the number of
    /// requests of transactions to bitcoin core
    #[arg(short, long, default_value = "100000", env)]
    pub tx_cache_size: usize,

    /// Some requests to the bitcoin core are concurrent, this set the desired parallelism.
    /// Note there is a limit of open files that this setting too high could trigger.
    /// See https://github.com/bitcoin/bitcoin/blob/master/doc/REST-interface.md#risks
    #[arg(short, long, default_value = "10", env)]
    pub fetch_parallelism: usize,

    /// default to "127.0.0.1:<port>" where port depend on the network used, eg 8332 for mainnnet.
    #[arg(short, long, env)]
    pub bitcoind_addr: Option<SocketAddr>,

    /// default value: bitcoin
    ///
    /// other possible values: testnet, signet
    #[arg(short, long, env)]
    pub network: Option<NetworkParse>,

    /// The socket address this service will bind on. Default value depends on the network:
    /// * mainnet: "127.0.0.1:3000"
    /// * testnet: "127.0.0.1:3001"
    /// * signet:  "127.0.0.1:3002"
    /// * regtest: "127.0.0.1:3003"

    #[arg(short, long, env)]
    pub local_addr: Option<SocketAddr>,

    /// If the setup involve multiple networks this must be set accordingly.
    /// An header with a link to all the network is generated.
    /// Links are prepended the network if it isn't mainet (eg `/testnet/t/xxx...`)
    /// Note the routes are still working without the network, it is duty of a frontend to redirect the
    /// path to appropriate port. eg.
    ///
    ///   ```no_build
    ///   location = /testnet {
    ///     return 302 /testnet/;
    ///   }
    ///   location /testnet/ {
    ///     proxy_pass http://10.0.0.7:3001/;
    ///   }
    ///   ```
    ///
    #[arg(short, long, env)]
    pub other_network: Vec<Network>,
}

pub async fn inner_main(mut args: Arguments) -> Result<(), Error> {
    init_globals(&mut args);

    let addr = args.local_addr.take().unwrap_or_else(|| match network() {
        Network::Bitcoin => create_local_socket(3000),
        Network::Testnet => create_local_socket(3001),
        Network::Signet => create_local_socket(3002),
        Network::Regtest => create_local_socket(3003),
        _ => panic!("non existing network"),
    });
    let args = args;

    log::debug!("local address {:?}", addr);

    let mut chain_info;
    loop {
        chain_info = match rpc::chaininfo::call().await {
            Ok(chain_info) => chain_info,
            Err(Error::RpcChainInfo(status_code)) if status_code == 404 => {
                return Err(Error::RestFlag);
            }
            Err(Error::RpcChainInfo(status_code)) if status_code == 503 => {
                log::warn!("bitcoind is still loading, waiting... (note: if on regtest you may need to generate a block to terminate IBD)");
                sleep(tokio::time::Duration::from_secs(10)).await;
                continue;
            }
            Err(e) => {
                let network = network();
                log::error!(
                    "bitcoind is probably not running, or running on wrong network {network}",
                );
                return Err(e);
            }
        };
        if chain_info.initial_block_download {
            log::warn!("bitcoind is not synced, waiting (on regtest you may need to generate a block)... {:?}", chain_info);
            sleep(tokio::time::Duration::from_secs(10)).await;
        } else {
            log::info!("bitcoind is synced: {:?}", chain_info);
            break;
        }
    }

    let core_net = Network::from_core_arg(chain_info.chain.as_str())?;
    check_network(core_net)?;

    let mempool_info = rpc::mempool::info().await?;
    log::info!("{:?}", mempool_info);

    let shared_state = Arc::new(SharedState::new(chain_info.clone(), args, mempool_info));

    // initialize cache with information from headers
    let shared_state_bootstrap = shared_state.clone();
    let h = tokio::spawn(async move { bootstrap_state_infallible(shared_state_bootstrap).await });

    // keep chain info updated
    let shared_state_chain = shared_state.clone();
    let shared_state_mempool = shared_state.clone();

    #[allow(clippy::let_underscore_future)]
    let _ = tokio::spawn(async move {
        h.await.unwrap();
        #[allow(clippy::let_underscore_future)]
        let _ = tokio::spawn(async move {
            update_chain_info_infallible(shared_state_chain, chain_info).await
        });
        update_mempool(shared_state_mempool).await
    });

    let make_service = make_service_fn(move |_| {
        let shared_state = shared_state.clone();

        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                let shared_state = shared_state.clone();
                route_infallible(req, shared_state)
            }))
        }
    });

    let server = Server::bind(&addr).serve(make_service);

    log::info!("Listening on http://{}", addr);

    if let Err(e) = server.with_graceful_shutdown(shutdown_signal()).await {
        log::error!("server error: {}", e);
    }
    Ok(())
}

async fn shutdown_signal() {
    // Wait for the CTRL+C signal
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
}

fn check_network(bitcoind: Network) -> Result<(), Error> {
    let fbbe = network();

    (fbbe == bitcoind)
        .then_some(())
        .ok_or(Error::WrongNetwork { fbbe, bitcoind })
}

trait NetworkExt {
    fn as_url_path(&self) -> NetworkPath;
    fn to_maiusc_string(&self) -> String;
}

pub struct NetworkPath(Network);

impl Display for NetworkPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Network::Bitcoin = self.0 {
            write!(f, "/")
        } else if networks().len() == 1 {
            write!(f, "/")
        } else {
            write!(f, "/{}/", self.0)
        }
    }
}

impl NetworkExt for Network {
    fn as_url_path(&self) -> NetworkPath {
        NetworkPath(*self)
    }

    fn to_maiusc_string(&self) -> String {
        format!("{:?}", self)
    }
}

pub fn create_local_socket(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
}
