//! Monitoring and metrics system
//!
//! Provides comprehensive monitoring including:
//! - Health checks
//! - Performance metrics
//! - Alert system
//! - HTTP metrics endpoint

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

use crate::utils::config::Config;

/// Monitoring system for bot health and metrics
#[derive(Debug)]
pub struct MonitoringSystem {
    config: Config,
    metrics: Arc<RwLock<MetricsStore>>,
    alerts: Arc<RwLock<Vec<Alert>>>,
    health_checks: Arc<RwLock<HashMap<String, HealthCheck>>>,
}

impl MonitoringSystem {
    /// Create new monitoring system
    pub fn new(config: Config) -> Self {
        let mut health_checks = HashMap::new();

        // Initialize core component health checks
        health_checks.insert("mempool_listener".to_string(), HealthCheck::new("mempool_listener"));
        health_checks.insert("strategy_router".to_string(), HealthCheck::new("strategy_router"));
        health_checks.insert("simulator".to_string(), HealthCheck::new("simulator"));
        health_checks.insert("executor".to_string(), HealthCheck::new("executor"));

        Self {
            config,
            metrics: Arc::new(RwLock::new(MetricsStore::new())),
            alerts: Arc::new(RwLock::new(Vec::new())),
            health_checks: Arc::new(RwLock::new(health_checks)),
        }
    }

    /// Record a metric
    pub async fn record_metric(&self, name: &str, value: f64, labels: Option<HashMap<String, String>>) {
        let mut metrics = self.metrics.write().await;
        metrics.record(name, value, labels);
    }

    /// Increment a counter metric
    pub async fn increment_counter(&self, name: &str, labels: Option<HashMap<String, String>>) {
        let mut metrics = self.metrics.write().await;
        metrics.increment_counter(name, labels);
    }

    /// Update component health
    pub async fn update_health(&self, component: &str, healthy: bool, message: Option<String>) {
        let mut health_checks = self.health_checks.write().await;
        if let Some(check) = health_checks.get_mut(component) {
            check.update(healthy, message);
        }
    }

    /// Record an alert
    pub async fn record_alert(&self, alert_type: AlertType, message: String, severity: AlertSeverity) {
        let alert = Alert {
            id: uuid::Uuid::new_v4().to_string(),
            alert_type,
            message,
            severity,
            timestamp: Utc::now(),
            acknowledged: false,
        };

        let mut alerts = self.alerts.write().await;
        alerts.push(alert.clone());

        // Keep only recent alerts (last 1000)
        if alerts.len() > 1000 {
            alerts.remove(0);
        }

        // Log alert
        match severity {
            AlertSeverity::Critical | AlertSeverity::Error => {
                tracing::error!("Alert: {} - {}", alert.alert_type.as_str(), alert.message);
            }
            AlertSeverity::Warning => {
                tracing::warn!("Alert: {} - {}", alert.alert_type.as_str(), alert.message);
            }
            AlertSeverity::Info => {
                tracing::info!("Alert: {} - {}", alert.alert_type.as_str(), alert.message);
            }
        }
    }

    /// Get overall system health
    pub async fn get_system_health(&self) -> SystemHealth {
        let health_checks = self.health_checks.read().await;
        let metrics = self.metrics.read().await;
        let alerts = self.alerts.read().await;

        let mut components_healthy = 0;
        let mut components_total = 0;
        let mut component_details = Vec::new();

        for (name, check) in health_checks.iter() {
            components_total += 1;
            if check.healthy {
                components_healthy += 1;
            }
            component_details.push(ComponentHealthDetail {
                name: name.clone(),
                healthy: check.healthy,
                last_check: check.last_check,
                message: check.message.clone(),
            });
        }

        let overall_healthy = components_healthy == components_total;

        // Get key metrics
        let opportunities_found = metrics.get_gauge("opportunities_found").unwrap_or(0.0);
        let opportunities_executed = metrics.get_gauge("opportunities_executed").unwrap_or(0.0);
        let success_rate = if opportunities_found > 0.0 {
            opportunities_executed / opportunities_found
        } else {
            0.0
        };

        let total_profit_usd = metrics.get_gauge("total_profit_usd").unwrap_or(0.0);
        let total_loss_usd = metrics.get_gauge("total_loss_usd").unwrap_or(0.0);

        // Get unacknowledged alerts
        let unacknowledged_alerts = alerts.iter()
            .filter(|a| !a.acknowledged)
            .cloned()
            .collect();

        SystemHealth {
            overall_healthy,
            components_healthy,
            components_total,
            component_details,
            opportunities_found: opportunities_found as u64,
            opportunities_executed: opportunities_executed as u64,
            success_rate,
            total_profit_usd,
            total_loss_usd,
            unacknowledged_alerts,
            uptime_seconds: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }
    }

    /// Get metrics in Prometheus format
    pub async fn get_prometheus_metrics(&self) -> String {
        let metrics = self.metrics.read().await;
        metrics.to_prometheus_format()
    }

    /// Acknowledge an alert
    pub async fn acknowledge_alert(&self, alert_id: &str) -> bool {
        let mut alerts = self.alerts.write().await;
        if let Some(alert) = alerts.iter_mut().find(|a| a.id == alert_id) {
            alert.acknowledged = true;
            true
        } else {
            false
        }
    }

    /// Clean up old metrics and alerts
    pub async fn cleanup(&self) {
        let mut metrics = self.metrics.write().await;
        metrics.cleanup();

        let mut alerts = self.alerts.write().await;
        let cutoff = Utc::now() - chrono::Duration::hours(24);
        alerts.retain(|a| a.timestamp > cutoff || !a.acknowledged);
    }
}

/// Metrics storage
#[derive(Debug)]
pub struct MetricsStore {
    gauges: HashMap<String, Metric>,
    counters: HashMap<String, Metric>,
    histograms: HashMap<String, Histogram>,
}

impl MetricsStore {
    pub fn new() -> Self {
        Self {
            gauges: HashMap::new(),
            counters: HashMap::new(),
            histograms: HashMap::new(),
        }
    }

    pub fn record(&mut self, name: &str, value: f64, labels: Option<HashMap<String, String>>) {
        let metric = self.gauges.entry(name.to_string()).or_insert_with(|| Metric {
            name: name.to_string(),
            value: 0.0,
            labels: HashMap::new(),
            timestamp: Utc::now(),
        });

        metric.value = value;
        metric.labels = labels.unwrap_or_default();
        metric.timestamp = Utc::now();
    }

    pub fn increment_counter(&mut self, name: &str, labels: Option<HashMap<String, String>>) {
        let metric = self.counters.entry(name.to_string()).or_insert_with(|| Metric {
            name: name.to_string(),
            value: 0.0,
            labels: HashMap::new(),
            timestamp: Utc::now(),
        });

        metric.value += 1.0;
        metric.labels = labels.unwrap_or_default();
        metric.timestamp = Utc::now();
    }

    pub fn get_gauge(&self, name: &str) -> Option<f64> {
        self.gauges.get(name).map(|m| m.value)
    }

    pub fn to_prometheus_format(&self) -> String {
        let mut output = String::new();

        // Gauges
        for metric in self.gauges.values() {
            output.push_str(&format!("# HELP {} {}\n", metric.name, metric.name));
            output.push_str(&format!("# TYPE {} gauge\n", metric.name));
            output.push_str(&format!("{} {}\n", metric.name, metric.value));
        }

        // Counters
        for metric in self.counters.values() {
            output.push_str(&format!("# HELP {} {}\n", metric.name, metric.name));
            output.push_str(&format!("# TYPE {} counter\n", metric.name));
            output.push_str(&format!("{} {}\n", metric.name, metric.value));
        }

        output
    }

    pub fn cleanup(&mut self) {
        let cutoff = Utc::now() - chrono::Duration::hours(1);

        self.gauges.retain(|_, m| m.timestamp > cutoff);
        self.counters.retain(|_, m| m.timestamp > cutoff);
        self.histograms.retain(|_, h| h.last_update > cutoff);
    }
}

/// Individual metric
#[derive(Debug, Clone)]
pub struct Metric {
    pub name: String,
    pub value: f64,
    pub labels: HashMap<String, String>,
    pub timestamp: DateTime<Utc>,
}

/// Histogram for latency measurements
#[derive(Debug, Clone)]
pub struct Histogram {
    pub name: String,
    pub buckets: Vec<(f64, u64)>,
    pub sum: f64,
    pub count: u64,
    pub last_update: DateTime<Utc>,
}

/// Health check for a component
#[derive(Debug, Clone)]
pub struct HealthCheck {
    pub component: String,
    pub healthy: bool,
    pub last_check: DateTime<Utc>,
    pub message: Option<String>,
}

impl HealthCheck {
    pub fn new(component: &str) -> Self {
        Self {
            component: component.to_string(),
            healthy: true,
            last_check: Utc::now(),
            message: None,
        }
    }

    pub fn update(&mut self, healthy: bool, message: Option<String>) {
        self.healthy = healthy;
        self.last_check = Utc::now();
        self.message = message;
    }
}

/// Alert system
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub id: String,
    pub alert_type: AlertType,
    pub message: String,
    pub severity: AlertSeverity,
    pub timestamp: DateTime<Utc>,
    pub acknowledged: bool,
}

/// Alert types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertType {
    SystemHealth,
    Performance,
    Risk,
    Trading,
    Network,
}

impl AlertType {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertType::SystemHealth => "system_health",
            AlertType::Performance => "performance",
            AlertType::Risk => "risk",
            AlertType::Trading => "trading",
            AlertType::Network => "network",
        }
    }
}

/// Alert severity levels
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertSeverity {
    Info,
    Warning,
    Error,
    Critical,
}

/// System health status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemHealth {
    pub overall_healthy: bool,
    pub components_healthy: u32,
    pub components_total: u32,
    pub component_details: Vec<ComponentHealthDetail>,
    pub opportunities_found: u64,
    pub opportunities_executed: u64,
    pub success_rate: f64,
    pub total_profit_usd: f64,
    pub total_loss_usd: f64,
    pub unacknowledged_alerts: Vec<Alert>,
    pub uptime_seconds: u64,
}

/// Component health detail
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentHealthDetail {
    pub name: String,
    pub healthy: bool,
    pub last_check: DateTime<Utc>,
    pub message: Option<String>,
}

/// HTTP server for metrics endpoint (optional feature)
#[cfg(feature = "monitoring-server")]
pub mod server {
    use super::*;
    use warp::Filter;

    pub async fn start_metrics_server(
        monitoring: Arc<MonitoringSystem>,
        port: u16,
    ) {
        let monitoring_filter = warp::any().map(move || monitoring.clone());

        let health_route = warp::path!("health")
            .and(monitoring_filter.clone())
            .and_then(health_handler);

        let metrics_route = warp::path!("metrics")
            .and(monitoring_filter)
            .and_then(metrics_handler);

        let routes = health_route.or(metrics_route);

        warp::serve(routes)
            .run(([127, 0, 0, 1], port))
            .await;
    }

    async fn health_handler(
        monitoring: Arc<MonitoringSystem>,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let health = monitoring.get_system_health().await;
        let status = if health.overall_healthy { 200 } else { 503 };

        Ok(warp::reply::with_status(
            warp::reply::json(&health),
            warp::http::StatusCode::from_u16(status).unwrap(),
        ))
    }

    async fn metrics_handler(
        monitoring: Arc<MonitoringSystem>,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let metrics = monitoring.get_prometheus_metrics().await;
        Ok(warp::reply::with_header(
            metrics,
            "content-type",
            "text/plain; version=0.0.4; charset=utf-8",
        ))
    }
}
