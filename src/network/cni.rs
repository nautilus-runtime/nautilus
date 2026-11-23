use crate::core::{Result, Error};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::process::Command as AsyncCommand;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CniConfig {
    pub cni_version: String,
    pub name: String,
    pub r#type: String,
    pub bridge: Option<String>,
    pub is_gateway: Option<bool>,
    pub ip_masq: Option<bool>,
    pub ipam: Option<IpamConfig>,
    pub capabilities: Option<HashMap<String, bool>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpamConfig {
    pub r#type: String,
    pub subnet: Option<String>,
    pub gateway: Option<String>,
    pub routes: Option<Vec<Route>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub dst: String,
    pub gw: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CniResult {
    pub cni_version: String,
    pub interfaces: Vec<CniInterface>,
    pub ips: Vec<CniIp>,
    pub routes: Option<Vec<Route>>,
    pub dns: Option<CniDns>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CniInterface {
    pub name: String,
    pub mac: String,
    pub sandbox: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CniIp {
    pub version: String,
    pub address: String,
    pub gateway: Option<String>,
    pub interface: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CniDns {
    pub nameservers: Option<Vec<String>>,
    pub domain: Option<String>,
    pub search: Option<Vec<String>>,
    pub options: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct CniPlugin {
    pub name: String,
    pub path: PathBuf,
}

impl CniPlugin {
    pub fn new(name: &str, path: &str) -> Self {
        Self {
            name: name.to_string(),
            path: PathBuf::from(path),
        }
    }
    
    pub async fn add_network(
        &self,
        config: &CniConfig,
        container_id: &str,
        netns_path: &str,
        interface_name: &str,
    ) -> Result<CniResult> {
        let config_json = serde_json::to_string(config)
            .map_err(|e| Error::Network(format!("Failed to serialize CNI config: {}", e)))?;
        
        let mut cmd = AsyncCommand::new(&self.path);
        cmd.env("CNI_COMMAND", "ADD")
            .env("CNI_CONTAINERID", container_id)
            .env("CNI_NETNS", netns_path)
            .env("CNI_IFNAME", interface_name)
            .env("CNI_PATH", "/opt/cni/bin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        
        let mut child = cmd.spawn()
            .map_err(|e| Error::Network(format!("Failed to spawn CNI plugin: {}", e)))?;
        
        if let Some(stdin) = child.stdin.as_mut() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(config_json.as_bytes()).await
                .map_err(|e| Error::Network(format!("Failed to write CNI config: {}", e)))?;
        }
        
        let output = child.wait_with_output().await
            .map_err(|e| Error::Network(format!("CNI plugin execution failed: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Network(format!("CNI plugin failed: {}", stderr)));
        }
        
        let result_json = String::from_utf8(output.stdout)
            .map_err(|e| Error::Network(format!("Invalid CNI output: {}", e)))?;
        
        let result: CniResult = serde_json::from_str(&result_json)
            .map_err(|e| Error::Network(format!("Failed to parse CNI result: {}", e)))?;
        
        Ok(result)
    }
    
    pub async fn del_network(
        &self,
        config: &CniConfig,
        container_id: &str,
        netns_path: &str,
        interface_name: &str,
    ) -> Result<()> {
        let config_json = serde_json::to_string(config)
            .map_err(|e| Error::Network(format!("Failed to serialize CNI config: {}", e)))?;
        
        let mut cmd = AsyncCommand::new(&self.path);
        cmd.env("CNI_COMMAND", "DEL")
            .env("CNI_CONTAINERID", container_id)
            .env("CNI_NETNS", netns_path)
            .env("CNI_IFNAME", interface_name)
            .env("CNI_PATH", "/opt/cni/bin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        
        let mut child = cmd.spawn()
            .map_err(|e| Error::Network(format!("Failed to spawn CNI plugin: {}", e)))?;
        
        if let Some(stdin) = child.stdin.as_mut() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(config_json.as_bytes()).await
                .map_err(|e| Error::Network(format!("Failed to write CNI config: {}", e)))?;
        }
        
        let output = child.wait_with_output().await
            .map_err(|e| Error::Network(format!("CNI plugin execution failed: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Network(format!("CNI plugin failed: {}", stderr)));
        }
        
        Ok(())
    }
    
    pub async fn check_network(
        &self,
        config: &CniConfig,
        container_id: &str,
        netns_path: &str,
        interface_name: &str,
    ) -> Result<()> {
        let config_json = serde_json::to_string(config)
            .map_err(|e| Error::Network(format!("Failed to serialize CNI config: {}", e)))?;
        
        let mut cmd = AsyncCommand::new(&self.path);
        cmd.env("CNI_COMMAND", "CHECK")
            .env("CNI_CONTAINERID", container_id)
            .env("CNI_NETNS", netns_path)
            .env("CNI_IFNAME", interface_name)
            .env("CNI_PATH", "/opt/cni/bin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        
        let mut child = cmd.spawn()
            .map_err(|e| Error::Network(format!("Failed to spawn CNI plugin: {}", e)))?;
        
        if let Some(stdin) = child.stdin.as_mut() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(config_json.as_bytes()).await
                .map_err(|e| Error::Network(format!("Failed to write CNI config: {}", e)))?;
        }
        
        let output = child.wait_with_output().await
            .map_err(|e| Error::Network(format!("CNI plugin execution failed: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Network(format!("CNI plugin failed: {}", stderr)));
        }
        
        Ok(())
    }
}

impl Default for CniConfig {
    fn default() -> Self {
        Self {
            cni_version: "0.4.0".to_string(),
            name: "default".to_string(),
            r#type: "bridge".to_string(),
            bridge: Some("cni0".to_string()),
            is_gateway: Some(true),
            ip_masq: Some(true),
            ipam: Some(IpamConfig {
                r#type: "host-local".to_string(),
                subnet: Some("172.17.0.0/16".to_string()),
                gateway: Some("172.17.0.1".to_string()),
                routes: Some(vec![Route {
                    dst: "0.0.0.0/0".to_string(),
                    gw: None,
                }]),
            }),
            capabilities: Some({
                let mut caps = HashMap::new();
                caps.insert("portMappings".to_string(), true);
                caps
            }),
        }
    }
}