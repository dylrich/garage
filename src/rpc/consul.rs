use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};

use err_derive::Error;
use serde::{Deserialize, Serialize};

use netapp::NodeID;

use garage_util::config::ConsulDiscoveryAPI;
use garage_util::config::ConsulDiscoveryConfig;

const META_PREFIX: &str = "fr-deuxfleurs-garagehq";

#[derive(Deserialize, Clone, Debug)]
struct ConsulQueryEntry {
	#[serde(rename = "Address")]
	address: String,
	#[serde(rename = "ServicePort")]
	service_port: u16,
	#[serde(rename = "NodeMeta")]
	node_meta: HashMap<String, String>,
	#[serde(rename = "ServiceMeta")]
	service_meta: HashMap<String, String>,
}

#[derive(Serialize, Clone, Debug)]
struct ConsulPublishEntry {
	#[serde(rename = "Node")]
	node: String,
	#[serde(rename = "Address")]
	address: IpAddr,
	#[serde(rename = "NodeMeta")]
	node_meta: HashMap<String, String>,
	#[serde(rename = "Service")]
	service: ConsulPublishCatalogService,
}

#[derive(Serialize, Clone, Debug)]
struct ConsulPublishCatalogService {
	#[serde(rename = "ID")]
	service_id: String,
	#[serde(rename = "Service")]
	service_name: String,
	#[serde(rename = "Tags")]
	tags: Vec<String>,
	#[serde(rename = "Meta")]
	service_meta: HashMap<String, String>,
	#[serde(rename = "Address")]
	address: IpAddr,
	#[serde(rename = "Port")]
	port: u16,
}

#[derive(Serialize, Clone, Debug)]
struct ConsulPublishService {
	#[serde(rename = "ID")]
	service_id: String,
	#[serde(rename = "Name")]
	service_name: String,
	#[serde(rename = "Tags")]
	tags: Vec<String>,
	#[serde(rename = "Address")]
	address: IpAddr,
	#[serde(rename = "Port")]
	port: u16,
	#[serde(rename = "Meta")]
	meta: HashMap<String, String>,
}

// ----
pub struct ConsulDiscovery {
	config: ConsulDiscoveryConfig,
	client: reqwest::Client,
}

impl ConsulDiscovery {
	pub fn new(config: ConsulDiscoveryConfig) -> Result<Self, ConsulError> {
		let mut builder: reqwest::ClientBuilder = reqwest::Client::builder();
		if config.tls_skip_verify {
			builder = builder.danger_accept_invalid_certs(true);
		} else if let Some(ca_cert) = &config.ca_cert {
			let mut ca_cert_buf = vec![];
			File::open(ca_cert)?.read_to_end(&mut ca_cert_buf)?;
			builder = builder.use_rustls_tls();
			builder =
				builder.add_root_certificate(reqwest::Certificate::from_pem(&ca_cert_buf[..])?);
		}

		let client: reqwest::Client = match &config.consul_http_api {
			ConsulDiscoveryAPI::Catalog => {
				match (&config.client_cert, &config.client_key) {
					(Some(client_cert), Some(client_key)) => {
						let mut client_cert_buf = vec![];
						File::open(client_cert)?.read_to_end(&mut client_cert_buf)?;

						let mut client_key_buf = vec![];
						File::open(client_key)?.read_to_end(&mut client_key_buf)?;

						let identity = reqwest::Identity::from_pem(
							&[&client_cert_buf[..], &client_key_buf[..]].concat()[..],
						)?;

						builder = builder.use_rustls_tls();
						builder = builder.identity(identity);
					}
					(None, None) => {}
					_ => return Err(ConsulError::InvalidTLSConfig),
				}

				builder.build()?
			}
			ConsulDiscoveryAPI::Agent => {
				if let Some(token) = &config.consul_http_token {
					let mut headers = reqwest::header::HeaderMap::new();
					headers.insert(
						"x-consul-token",
						reqwest::header::HeaderValue::from_str(&token)?,
					);
					builder = builder.default_headers(headers);
				}

				builder.build()?
			}
		};

		Ok(Self { client, config })
	}

	// ---- READING FROM CONSUL CATALOG ----

	pub async fn get_consul_nodes(&self) -> Result<Vec<(NodeID, SocketAddr)>, ConsulError> {
		let url = format!(
			"{}/v1/catalog/service/{}",
			self.config.consul_http_addr, self.config.service_name
		);

		let http = self.client.get(&url).send().await?;
		let entries: Vec<ConsulQueryEntry> = http.json().await?;

		let mut ret = vec![];
		for ent in entries {
			let ip = ent.address.parse::<IpAddr>().ok();
			let pubkey = match &self.config.consul_http_api {
				ConsulDiscoveryAPI::Catalog => ent.node_meta.get("pubkey"),
				ConsulDiscoveryAPI::Agent => {
					ent.service_meta.get(&format!("{}-pubkey", META_PREFIX))
				}
			}
			.and_then(|k| hex::decode(k).ok())
			.and_then(|k| NodeID::from_slice(&k[..]));
			if let (Some(ip), Some(pubkey)) = (ip, pubkey) {
				ret.push((pubkey, SocketAddr::new(ip, ent.service_port)));
			} else {
				warn!(
					"Could not process node spec from Consul: {:?} (invalid IP or public key)",
					ent
				);
			}
		}
		debug!("Got nodes from Consul: {:?}", ret);

		Ok(ret)
	}

	// ---- PUBLISHING TO CONSUL CATALOG ----

	pub async fn publish_consul_service(
		&self,
		node_id: NodeID,
		hostname: &str,
		rpc_public_addr: SocketAddr,
	) -> Result<(), ConsulError> {
		let node = format!("garage:{}", hex::encode(&node_id[..8]));
		let tags = [
			vec!["advertised-by-garage".into(), hostname.into()],
			self.config.tags.clone(),
		]
		.concat();

		let meta_prefix: String = match &self.config.consul_http_api {
			ConsulDiscoveryAPI::Catalog => "".to_string(),
			ConsulDiscoveryAPI::Agent => format!("{}-", META_PREFIX),
		};

		let mut meta = HashMap::from([
			(format!("{}pubkey", meta_prefix), hex::encode(node_id)),
			(format!("{}hostname", meta_prefix), hostname.to_string()),
		]);

		if let Some(global_meta) = &self.config.meta {
			for (key, value) in global_meta.into_iter() {
				meta.insert(key.clone(), value.clone());
			}
		}

		let url = format!(
			"{}/v1/{}",
			self.config.consul_http_addr,
			(match &self.config.consul_http_api {
				ConsulDiscoveryAPI::Catalog => "catalog/register",
				ConsulDiscoveryAPI::Agent => "agent/service/register?replace-existing-checks",
			})
		);

		let req = self.client.put(&url);
		let http = (match &self.config.consul_http_api {
			ConsulDiscoveryAPI::Catalog => req.json(&ConsulPublishEntry {
				node: node.clone(),
				address: rpc_public_addr.ip(),
				node_meta: meta.clone(),
				service: ConsulPublishCatalogService {
					service_id: node.clone(),
					service_name: self.config.service_name.clone(),
					tags,
					service_meta: meta.clone(),
					address: rpc_public_addr.ip(),
					port: rpc_public_addr.port(),
				},
			}),
			ConsulDiscoveryAPI::Agent => req.json(&ConsulPublishService {
				service_id: node.clone(),
				service_name: self.config.service_name.clone(),
				tags,
				meta,
				address: rpc_public_addr.ip(),
				port: rpc_public_addr.port(),
			}),
		})
		.send()
		.await?;
		http.error_for_status()?;

		Ok(())
	}
}

/// Regroup all Consul discovery errors
#[derive(Debug, Error)]
pub enum ConsulError {
	#[error(display = "IO error: {}", _0)]
	Io(#[error(source)] std::io::Error),
	#[error(display = "HTTP error: {}", _0)]
	Reqwest(#[error(source)] reqwest::Error),
	#[error(display = "Invalid Consul TLS configuration")]
	InvalidTLSConfig,
	#[error(display = "Token error: {}", _0)]
	Token(#[error(source)] reqwest::header::InvalidHeaderValue),
}
