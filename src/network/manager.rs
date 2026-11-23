use crate::core::{Config, Result, Error};
use crate::network::{CniPlugin, BridgeNetwork};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInfo {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub subnet: String,
    pub gateway: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub containers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkEndpoint {
    pub network_id: String,
    pub container_id: String,
    pub ip_address: String,
    pub mac_address: String,
    pub interface_name: String,
}

#[derive(Debug)]
pub struct NetworkManager {
    _config: Arc<Config>,
    networks: Arc<RwLock<HashMap<String, NetworkInfo>>>,
    endpoints: Arc<RwLock<HashMap<String, Vec<NetworkEndpoint>>>>,
    _cni_plugins: Vec<CniPlugin>,
    bridge_network: BridgeNetwork,
}

impl NetworkManager {
    pub fn new(config: Arc<Config>) -> Self {
        let cni_plugins = Self::load_cni_plugins(&config.network.cni_config_dir);
        let bridge_network = BridgeNetwork::new(config.clone());
        
        Self {
            _config: config,
            networks: Arc::new(RwLock::new(HashMap::new())),
            endpoints: Arc::new(RwLock::new(HashMap::new())),
            _cni_plugins: cni_plugins,
            bridge_network,
        }
    }
    
    pub async fn initialize(&self) -> Result<()> {
        // Initialize default bridge network
        self.bridge_network.initialize().await?;
        
        // Create default network
        let default_network = NetworkInfo {
            id: "default".to_string(),
            name: "default".to_string(),
            driver: "bridge".to_string(),
            subnet: "172.17.0.0/16".to_string(),
            gateway: "172.17.0.1".to_string(),
            created_at: chrono::Utc::now(),
            containers: Vec::new(),
        };
        
        let mut networks = self.networks.write().await;
        networks.insert("default".to_string(), default_network);
        
        Ok(())
    }
    
    pub async fn create_network(
        &self,
        name: &str,
        driver: &str,
        subnet: Option<&str>,
        gateway: Option<&str>,
    ) -> Result<String> {
        let network_id = uuid::Uuid::new_v4().to_string();
        
        let subnet = subnet.unwrap_or("172.18.0.0/16");
        let gateway = gateway.unwrap_or("172.18.0.1");
        
        match driver {
            "bridge" => {
                self.bridge_network.create_network(&network_id, subnet, gateway).await?;
            }
            _ => {
                return Err(Error::Network(format!("Unsupported network driver: {}", driver)));
            }
        }
        
        let network_info = NetworkInfo {
            id: network_id.clone(),
            name: name.to_string(),
            driver: driver.to_string(),
            subnet: subnet.to_string(),
            gateway: gateway.to_string(),
            created_at: chrono::Utc::now(),
            containers: Vec::new(),
        };
        
        let mut networks = self.networks.write().await;
        networks.insert(network_id.clone(), network_info);
        
        Ok(network_id)
    }
    
    pub async fn remove_network(&self, network_id: &str) -> Result<()> {
        if network_id == "default" {
            return Err(Error::Network("Cannot remove default network".to_string()));
        }
        
        let mut networks = self.networks.write().await;
        // Docker parity: accept the network id or its name.
        let target_id = if networks.contains_key(network_id) {
            network_id.to_string()
        } else {
            networks
                .values()
                .find(|n| n.name == network_id)
                .map(|n| n.id.clone())
                .ok_or_else(|| Error::Network(format!("Network not found: {}", network_id)))?
        };
        let network = networks.get(&target_id)
            .ok_or_else(|| Error::Network(format!("Network not found: {}", target_id)))?;
        
        if !network.containers.is_empty() {
            return Err(Error::Network("Cannot remove network with connected containers".to_string()));
        }
        
        match network.driver.as_str() {
            "bridge" => {
                self.bridge_network.remove_network(&target_id).await?;
            }
            _ => {
                return Err(Error::Network(format!("Unsupported network driver: {}", network.driver)));
            }
        }
        
        networks.remove(&target_id);
        Ok(())
    }
    
    pub async fn list_networks(&self) -> Result<Vec<NetworkInfo>> {
        let networks = self.networks.read().await;
        Ok(networks.values().cloned().collect())
    }
    
    pub async fn get_network(&self, network_id: &str) -> Result<Option<NetworkInfo>> {
        let networks = self.networks.read().await;
        Ok(networks.get(network_id).cloned())
    }
    
    pub async fn connect_container(
        &self,
        container_id: &str,
        network_id: &str,
    ) -> Result<NetworkEndpoint> {
        let mut networks = self.networks.write().await;
        let network = networks.get_mut(network_id)
            .ok_or_else(|| Error::Network(format!("Network not found: {}", network_id)))?;
        
        // Generate IP address for container
        let ip_address = self.allocate_ip_address(network_id).await?;
        let mac_address = self.generate_mac_address();
        let interface_name = format!("eth{}", self.get_next_interface_index(container_id).await?);
        
        let endpoint = NetworkEndpoint {
            network_id: network_id.to_string(),
            container_id: container_id.to_string(),
            ip_address: ip_address.clone(),
            mac_address: mac_address.clone(),
            interface_name: interface_name.clone(),
        };
        
        match network.driver.as_str() {
            "bridge" => {
                self.bridge_network.connect_container(
                    container_id,
                    network_id,
                    &ip_address,
                    &mac_address,
                    &interface_name,
                ).await?;
            }
            _ => {
                return Err(Error::Network(format!("Unsupported network driver: {}", network.driver)));
            }
        }
        
        // Update network container list
        network.containers.push(container_id.to_string());
        
        // Store endpoint
        let mut endpoints = self.endpoints.write().await;
        endpoints.entry(container_id.to_string())
            .or_insert_with(Vec::new)
            .push(endpoint.clone());
        
        Ok(endpoint)
    }
    
    pub async fn disconnect_container(
        &self,
        container_id: &str,
        network_id: &str,
    ) -> Result<()> {
        let mut networks = self.networks.write().await;
        let network = networks.get_mut(network_id)
            .ok_or_else(|| Error::Network(format!("Network not found: {}", network_id)))?;
        
        // Remove container from network
        network.containers.retain(|id| id != container_id);
        
        // Remove endpoint
        let mut endpoints = self.endpoints.write().await;
        if let Some(container_endpoints) = endpoints.get_mut(container_id) {
            container_endpoints.retain(|ep| ep.network_id != network_id);
            if container_endpoints.is_empty() {
                endpoints.remove(container_id);
            }
        }
        
        match network.driver.as_str() {
            "bridge" => {
                self.bridge_network.disconnect_container(container_id, network_id).await?;
            }
            _ => {
                return Err(Error::Network(format!("Unsupported network driver: {}", network.driver)));
            }
        }
        
        Ok(())
    }
    
    pub async fn get_container_endpoints(&self, container_id: &str) -> Result<Vec<NetworkEndpoint>> {
        let endpoints = self.endpoints.read().await;
        Ok(endpoints.get(container_id).cloned().unwrap_or_default())
    }
    
    fn load_cni_plugins(_cni_config_dir: &PathBuf) -> Vec<CniPlugin> {
        // In a real implementation, this would scan the CNI config directory
        // and load available plugins
        vec![
            CniPlugin::new("bridge", "/opt/cni/bin/bridge"),
            CniPlugin::new("host-local", "/opt/cni/bin/host-local"),
            CniPlugin::new("portmap", "/opt/cni/bin/portmap"),
        ]
    }
    
    async fn allocate_ip_address(&self, network_id: &str) -> Result<String> {
        // Simple IP allocation - in a real implementation, this would be more sophisticated
        let networks = self.networks.read().await;
        let network = networks.get(network_id)
            .ok_or_else(|| Error::Network(format!("Network not found: {}", network_id)))?;
        
        // Parse subnet and allocate next available IP
        let subnet_parts: Vec<&str> = network.subnet.split('/').collect();
        if subnet_parts.len() != 2 {
            return Err(Error::Network("Invalid subnet format".to_string()));
        }
        
        let base_ip = subnet_parts[0];
        let ip_parts: Vec<&str> = base_ip.split('.').collect();
        if ip_parts.len() != 4 {
            return Err(Error::Network("Invalid IP format".to_string()));
        }
        
        // Simple allocation: increment last octet
        let last_octet: u8 = ip_parts[3].parse()
            .map_err(|_| Error::Network("Invalid IP format".to_string()))?;
        
        let allocated_ip = format!("{}.{}.{}.{}", ip_parts[0], ip_parts[1], ip_parts[2], last_octet + 2);
        Ok(allocated_ip)
    }
    
    fn generate_mac_address(&self) -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        format!(
            "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            rng.gen::<u8>(),
            rng.gen::<u8>(),
            rng.gen::<u8>(),
            rng.gen::<u8>(),
            rng.gen::<u8>()
        )
    }
    
    async fn get_next_interface_index(&self, container_id: &str) -> Result<usize> {
        let endpoints = self.endpoints.read().await;
        let empty_vec = Vec::new();
        let container_endpoints = endpoints.get(container_id).unwrap_or(&empty_vec);
        Ok(container_endpoints.len())
    }
}