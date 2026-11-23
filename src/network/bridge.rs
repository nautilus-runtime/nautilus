use crate::core::{Config, Result, Error};
use std::sync::Arc;
use tokio::process::Command;
use std::collections::HashMap;

#[derive(Debug)]
pub struct BridgeNetwork {
    _config: Arc<Config>,
    _bridges: HashMap<String, BridgeInfo>,
}

#[derive(Debug, Clone)]
struct BridgeInfo {
    _name: String,
    _subnet: String,
    _gateway: String,
    _interface_name: String,
}

impl BridgeNetwork {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            _config: config,
            _bridges: HashMap::new(),
        }
    }
    
    pub async fn initialize(&self) -> Result<()> {
        // Check if bridge utilities are available
        self.check_bridge_utils().await?;
        
        // Create default bridge if it doesn't exist
        self.ensure_default_bridge().await?;
        
        Ok(())
    }
    
    pub async fn create_network(
        &self,
        network_id: &str,
        subnet: &str,
        gateway: &str,
    ) -> Result<()> {
        let bridge_name = format!("br-{}", &network_id[..12]);
        
        // Create bridge interface
        self.create_bridge_interface(&bridge_name).await?;
        
        // Configure bridge IP
        self.configure_bridge_ip(&bridge_name, gateway, subnet).await?;
        
        // Enable bridge
        self.enable_bridge(&bridge_name).await?;
        
        // Setup iptables rules for NAT
        self.setup_nat_rules(&bridge_name, subnet).await?;
        
        Ok(())
    }
    
    pub async fn remove_network(&self, network_id: &str) -> Result<()> {
        let bridge_name = format!("br-{}", &network_id[..12]);
        
        // Remove iptables rules
        self.remove_nat_rules(&bridge_name).await?;
        
        // Remove bridge interface
        self.remove_bridge_interface(&bridge_name).await?;
        
        Ok(())
    }
    
    pub async fn connect_container(
        &self,
        container_id: &str,
        network_id: &str,
        ip_address: &str,
        mac_address: &str,
        interface_name: &str,
    ) -> Result<()> {
        let _ = (ip_address, mac_address, interface_name); // Suppress unused warnings until implementation is complete
        let bridge_name = format!("br-{}", &network_id[..12]);
        let veth_host = format!("veth{}", &container_id[..8]);
        let veth_container = format!("veth{}c", &container_id[..8]);
        
        // Create veth pair
        self.create_veth_pair(&veth_host, &veth_container).await?;
        
        // Attach host side to bridge
        self.attach_to_bridge(&bridge_name, &veth_host).await?;
        
        // Configure container side (this would typically be done inside the container namespace)
        // For now, we'll just bring up the host side
        self.bring_up_interface(&veth_host).await?;
        
        Ok(())
    }
    
    pub async fn disconnect_container(
        &self,
        container_id: &str,
        network_id: &str,
    ) -> Result<()> {
        let _ = network_id; // Suppress unused warning until implementation is complete
        let veth_host = format!("veth{}", &container_id[..8]);
        
        // Remove veth interface (this will also remove the pair)
        self.remove_interface(&veth_host).await?;
        
        Ok(())
    }
    
    async fn check_bridge_utils(&self) -> Result<()> {
        let output = Command::new("which")
            .arg("ip")
            .output()
            .await
            .map_err(|e| Error::Network(format!("Failed to check for ip command: {}", e)))?;
        
        if !output.status.success() {
            return Err(Error::Network("ip command not found. Please install iproute2".to_string()));
        }
        
        Ok(())
    }
    
    async fn ensure_default_bridge(&self) -> Result<()> {
        // Check if default bridge exists
        let output = Command::new("ip")
            .args(["link", "show", "cni0"])
            .output()
            .await
            .map_err(|e| Error::Network(format!("Failed to check bridge: {}", e)))?;
        
        if !output.status.success() {
            // Create default bridge
            self.create_bridge_interface("cni0").await?;
            self.configure_bridge_ip("cni0", "172.17.0.1", "172.17.0.0/16").await?;
            self.enable_bridge("cni0").await?;
            self.setup_nat_rules("cni0", "172.17.0.0/16").await?;
        }
        
        Ok(())
    }
    
    async fn create_bridge_interface(&self, bridge_name: &str) -> Result<()> {
        let output = Command::new("ip")
            .args(["link", "add", "name", bridge_name, "type", "bridge"])
            .output()
            .await
            .map_err(|e| Error::Network(format!("Failed to create bridge: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Network(format!("Failed to create bridge {}: {}", bridge_name, stderr)));
        }
        
        Ok(())
    }
    
    async fn configure_bridge_ip(&self, bridge_name: &str, gateway: &str, subnet: &str) -> Result<()> {
        let cidr = if gateway.contains('/') {
            gateway.to_string()
        } else {
            // Extract CIDR from subnet
            let prefix = subnet.split('/').nth(1).unwrap_or("16");
            format!("{}/{}", gateway, prefix)
        };
        
        let output = Command::new("ip")
            .args(["addr", "add", &cidr, "dev", bridge_name])
            .output()
            .await
            .map_err(|e| Error::Network(format!("Failed to configure bridge IP: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Ignore "File exists" error as it means IP is already configured
            if !stderr.contains("File exists") {
                return Err(Error::Network(format!("Failed to configure bridge IP {}: {}", bridge_name, stderr)));
            }
        }
        
        Ok(())
    }
    
    async fn enable_bridge(&self, bridge_name: &str) -> Result<()> {
        let output = Command::new("ip")
            .args(["link", "set", "dev", bridge_name, "up"])
            .output()
            .await
            .map_err(|e| Error::Network(format!("Failed to enable bridge: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Network(format!("Failed to enable bridge {}: {}", bridge_name, stderr)));
        }
        
        Ok(())
    }
    
    async fn setup_nat_rules(&self, bridge_name: &str, subnet: &str) -> Result<()> {
        // Enable IP forwarding
        let _ = Command::new("sysctl")
            .args(["-w", "net.ipv4.ip_forward=1"])
            .output()
            .await;
        
        // Add MASQUERADE rule for outbound traffic
        let output = Command::new("iptables")
            .args([
                "-t", "nat",
                "-A", "POSTROUTING",
                "-s", subnet,
                "!", "-o", bridge_name,
                "-j", "MASQUERADE"
            ])
            .output()
            .await
            .map_err(|e| Error::Network(format!("Failed to setup NAT rules: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Ignore if rule already exists
            if !stderr.contains("already exists") && !stderr.contains("Chain already exists") {
                return Err(Error::Network(format!("Failed to setup NAT rules: {}", stderr)));
            }
        }
        
        // Add FORWARD rules
        let _ = Command::new("iptables")
            .args([
                "-A", "FORWARD",
                "-i", bridge_name,
                "-o", bridge_name,
                "-j", "ACCEPT"
            ])
            .output()
            .await;
        
        Ok(())
    }
    
    async fn remove_nat_rules(&self, bridge_name: &str) -> Result<()> {
        // Remove MASQUERADE rule (ignore errors as rule might not exist)
        let _ = Command::new("iptables")
            .args([
                "-t", "nat",
                "-D", "POSTROUTING",
                "-o", bridge_name,
                "-j", "MASQUERADE"
            ])
            .output()
            .await;
        
        // Remove FORWARD rules
        let _ = Command::new("iptables")
            .args([
                "-D", "FORWARD",
                "-i", bridge_name,
                "-o", bridge_name,
                "-j", "ACCEPT"
            ])
            .output()
            .await;
        
        Ok(())
    }
    
    async fn remove_bridge_interface(&self, bridge_name: &str) -> Result<()> {
        // Bring down the bridge first
        let _ = Command::new("ip")
            .args(["link", "set", "dev", bridge_name, "down"])
            .output()
            .await;
        
        // Remove the bridge
        let output = Command::new("ip")
            .args(["link", "delete", bridge_name])
            .output()
            .await
            .map_err(|e| Error::Network(format!("Failed to remove bridge: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Ignore "Cannot find device" error
            if !stderr.contains("Cannot find device") {
                return Err(Error::Network(format!("Failed to remove bridge {}: {}", bridge_name, stderr)));
            }
        }
        
        Ok(())
    }
    
    async fn create_veth_pair(&self, veth_host: &str, veth_container: &str) -> Result<()> {
        let output = Command::new("ip")
            .args([
                "link", "add", "name", veth_host,
                "type", "veth",
                "peer", "name", veth_container
            ])
            .output()
            .await
            .map_err(|e| Error::Network(format!("Failed to create veth pair: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Network(format!("Failed to create veth pair: {}", stderr)));
        }
        
        Ok(())
    }
    
    async fn attach_to_bridge(&self, bridge_name: &str, interface: &str) -> Result<()> {
        let output = Command::new("ip")
            .args(["link", "set", "dev", interface, "master", bridge_name])
            .output()
            .await
            .map_err(|e| Error::Network(format!("Failed to attach to bridge: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Network(format!("Failed to attach {} to bridge {}: {}", interface, bridge_name, stderr)));
        }
        
        Ok(())
    }
    
    async fn bring_up_interface(&self, interface: &str) -> Result<()> {
        let output = Command::new("ip")
            .args(["link", "set", "dev", interface, "up"])
            .output()
            .await
            .map_err(|e| Error::Network(format!("Failed to bring up interface: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Network(format!("Failed to bring up interface {}: {}", interface, stderr)));
        }
        
        Ok(())
    }
    
    async fn remove_interface(&self, interface: &str) -> Result<()> {
        let output = Command::new("ip")
            .args(["link", "delete", interface])
            .output()
            .await
            .map_err(|e| Error::Network(format!("Failed to remove interface: {}", e)))?;
        
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Ignore "Cannot find device" error
            if !stderr.contains("Cannot find device") {
                return Err(Error::Network(format!("Failed to remove interface {}: {}", interface, stderr)));
            }
        }
        
        Ok(())
    }
}