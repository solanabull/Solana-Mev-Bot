use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{interval, sleep};
use reqwest::Client;
use anyhow::Result;
use colored::Colorize;
use lazy_static::lazy_static;
use dashmap::DashMap;
use crate::common::logger::Logger;
use crate::common::config::TransactionLandingMode;

// Global state for health checks
lazy_static! {
    static ref HEALTH_STATUS: Arc<DashMap<String, HealthStatus>> = Arc::new(DashMap::new());
}

#[derive(Debug, Clone)]
pub struct HealthStatus {
    pub service_name: String,
    pub is_healthy: bool,
    pub last_check: Instant,
    pub response_time_ms: u64,
    pub error_count: u32,
    pub consecutive_failures: u32,
}

impl HealthStatus {
    pub fn new(service_name: String) -> Self {
        Self {
            service_name,
            is_healthy: true,
            last_check: Instant::now(),
            response_time_ms: 0,
            error_count: 0,
            consecutive_failures: 0,
        }
    }
}

pub struct HealthCheckManager {
    logger: Logger,
    zeroslot_client: Arc<Client>,
}

impl HealthCheckManager {
    pub fn new(
        zeroslot_client: Arc<Client>,
    ) -> Self {
        Self {
            logger: Logger::new("[HEALTH-CHECK] => ".blue().to_string()),
            zeroslot_client,
        }
    }

    /// Start health check monitoring for Zeroslot service
    pub async fn start_monitoring(&self) -> Result<()> {
        self.logger.log("Starting health check monitoring for Zeroslot RPC service".green().to_string());

        // Initialize health status for Zeroslot service
        HEALTH_STATUS.insert("zeroslot".to_string(), HealthStatus::new("zeroslot".to_string()));

        // Start monitoring task
        self.start_zeroslot_monitoring().await;

        Ok(())
    }

    /// Start Zeroslot monitoring with 65-second keepalive
    async fn start_zeroslot_monitoring(&self) {
        let client = self.zeroslot_client.clone();
        let logger = self.logger.clone();
        
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(60)); // 60 seconds to be safe
            
            loop {
                interval.tick().await;
                
                let start_time = Instant::now();
                let result = Self::check_zeroslot_health(&client).await;
                let response_time = start_time.elapsed().as_millis() as u64;
                
                Self::update_health_status("zeroslot", result.is_ok(), response_time, &logger).await;
                
                if let Err(e) = result {
                    logger.log(format!("Zeroslot health check failed: {}", e).red().to_string());
                } else {
                    logger.log(format!("Zeroslot keepalive successful ({}ms)", response_time).green().to_string());
                }
            }
        });
    }









    /// Check Zeroslot health with keepalive
    async fn check_zeroslot_health(client: &Client) -> Result<()> {
        let health_url = crate::common::config::get_zero_slot_health_url();
        let response = client
            .get(&health_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        
        if response.status().is_success() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("Zeroslot health check failed with status: {}", response.status()))
        }
    }









    /// Update health status for a service
    async fn update_health_status(service_name: &str, is_healthy: bool, response_time_ms: u64, logger: &Logger) {
        let mut status = HEALTH_STATUS.entry(service_name.to_string())
            .or_insert_with(|| HealthStatus::new(service_name.to_string()));
        
        status.last_check = Instant::now();
        status.response_time_ms = response_time_ms;
        
        if is_healthy {
            status.is_healthy = true;
            status.consecutive_failures = 0;
        } else {
            status.is_healthy = false;
            status.error_count += 1;
            status.consecutive_failures += 1;
            
            if status.consecutive_failures >= 3 {
                logger.log(format!("Service {} is unhealthy after {} consecutive failures", 
                    service_name, status.consecutive_failures).red().bold().to_string());
            }
        }
    }

    /// Get health status for a service
    pub fn get_health_status(service_name: &str) -> Option<HealthStatus> {
        HEALTH_STATUS.get(service_name).map(|status| status.clone())
    }

    /// Get the healthiest service for a given transaction landing mode
    pub fn get_healthiest_service(landing_mode: &TransactionLandingMode) -> Option<String> {
        match landing_mode {
            TransactionLandingMode::Zeroslot => {
                if Self::is_service_healthy("zeroslot") { Some("zeroslot".to_string()) } else { None }
            },
            _ => {
                // Default to Zeroslot for any other mode
                if Self::is_service_healthy("zeroslot") { Some("zeroslot".to_string()) } else { None }
            }
        }
    }



    /// Check if a service is healthy
    fn is_service_healthy(service_name: &str) -> bool {
        HEALTH_STATUS.get(service_name)
            .map(|status| status.is_healthy)
            .unwrap_or(false)
    }

    /// Log health status for Zeroslot service
    pub fn log_all_health_status(logger: &Logger) {
        logger.log("=== Zeroslot RPC Service Health Status ===".blue().bold().to_string());
        
        for entry in HEALTH_STATUS.iter() {
            let service = entry.key();
            let status = entry.value();
            
            let health_indicator = if status.is_healthy { "✅" } else { "❌" };
            let color = if status.is_healthy { "green" } else { "red" };
            
            logger.log(format!(
                "{} {}: {} ({}ms, {} failures)",
                health_indicator,
                service,
                if status.is_healthy { "HEALTHY" } else { "UNHEALTHY" },
                status.response_time_ms,
                status.consecutive_failures
            ).color(color).to_string());
        }
    }

    /// Wait for service to become healthy
    pub async fn wait_for_service_health(service_name: &str, timeout_secs: u64) -> bool {
        let start_time = Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        
        while start_time.elapsed() < timeout {
            if Self::is_service_healthy(service_name) {
                return true;
            }
            
            sleep(Duration::from_secs(1)).await;
        }
        
        false
    }
} 