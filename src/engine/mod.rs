//! Core MEV bot engine
//!
//! This module contains the main orchestration logic for the MEV bot,
//! including mempool monitoring, strategy execution, and transaction management.

pub mod mempool_listener;
pub mod strategy_router;
pub mod simulation;
pub mod executor;

use std::sync::Arc;
use tokio::sync::RwLock;
use solana_client::rpc_client::RpcClient;

use crate::utils::config::Config;
use crate::utils::types::{ComponentHealth, EngineHealth};
use crate::strategies::{ArbitrageStrategy, SandwichStrategy, LiquidationStrategy};
use crate::dex::{DexManager, RaydiumDex, OrcaDex, OpenBookDex};

/// Engine configuration
#[derive(Debug)]
pub struct EngineConfig {
    pub solana_client: Arc<RpcClient>,
    pub config: Config,
}

/// Main MEV bot engine
#[derive(Debug)]
pub struct Engine {
    config: Config,
    solana_client: Arc<RpcClient>,
    mempool_listener: Arc<RwLock<MempoolListener>>,
    strategy_router: Arc<RwLock<StrategyRouter>>,
    simulator: Arc<RwLock<SimulationEngine>>,
    executor: Arc<RwLock<Executor>>,
    dex_manager: Arc<RwLock<DexManager>>,
    running: Arc<RwLock<bool>>,
}

impl Engine {
    /// Create new engine instance
    pub async fn new(config: EngineConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let solana_client = config.solana_client;
        let config = config.config;

        // Initialize DEX manager
        let dex_manager = Arc::new(RwLock::new(DexManager::new(&config).await?));

        // Initialize strategies
        let arbitrage_strategy = if config.strategies.arbitrage {
            Some(Arc::new(RwLock::new(ArbitrageStrategy::new(
                solana_client.clone(),
                dex_manager.clone(),
                config.clone(),
            ).await?)))
        } else {
            None
        };

        let sandwich_strategy = if config.strategies.sandwich {
            Some(Arc::new(RwLock::new(SandwichStrategy::new(
                solana_client.clone(),
                dex_manager.clone(),
                config.clone(),
            ).await?)))
        } else {
            None
        };

        let liquidation_strategy = if config.strategies.liquidation {
            Some(Arc::new(RwLock::new(LiquidationStrategy::new(
                solana_client.clone(),
                dex_manager.clone(),
                config.clone(),
            ).await?)))
        } else {
            None
        };

        // Initialize components
        let mempool_listener = Arc::new(RwLock::new(
            MempoolListener::new(solana_client.clone(), config.clone()).await?
        ));

        let strategy_router = Arc::new(RwLock::new(
            StrategyRouter::new(
                arbitrage_strategy,
                sandwich_strategy,
                liquidation_strategy,
                config.clone(),
            ).await?
        ));

        let simulator = Arc::new(RwLock::new(
            SimulationEngine::new(solana_client.clone(), config.clone()).await?
        ));

        let executor = Arc::new(RwLock::new(
            Executor::new(solana_client.clone(), config.clone()).await?
        ));

        Ok(Self {
            config,
            solana_client,
            mempool_listener,
            strategy_router,
            simulator,
            executor,
            dex_manager,
            running: Arc::new(RwLock::new(false)),
        })
    }

    /// Start the MEV engine
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        *self.running.write().await = true;

        tracing::info!("Starting MEV engine with strategies: arbitrage={}, sandwich={}, liquidation={}",
            self.config.strategies.arbitrage,
            self.config.strategies.sandwich,
            self.config.strategies.liquidation
        );

        // Start mempool listener
        let mempool_handle = {
            let mempool_listener = self.mempool_listener.clone();
            let running = self.running.clone();
            tokio::spawn(async move {
                let mut listener = mempool_listener.write().await;
                while *running.read().await {
                    if let Err(e) = listener.listen().await {
                        tracing::error!("Mempool listener error: {}", e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    }
                }
            })
        };

        // Start strategy router
        let router_handle = {
            let strategy_router = self.strategy_router.clone();
            let mempool_listener = self.mempool_listener.clone();
            let simulator = self.simulator.clone();
            let executor = self.executor.clone();
            let running = self.running.clone();

            tokio::spawn(async move {
                let mut router = strategy_router.write().await;
                while *running.read().await {
                    if let Err(e) = router.process_opportunities(
                        &mempool_listener,
                        &simulator,
                        &executor,
                    ).await {
                        tracing::error!("Strategy router error: {}", e);
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    }
                }
            })
        };

        // Wait for components to complete
        tokio::try_join!(mempool_handle, router_handle)?;

        Ok(())
    }

    /// Stop the MEV engine
    pub async fn stop(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        tracing::info!("Stopping MEV engine");
        *self.running.write().await = false;

        // Stop all components
        self.mempool_listener.write().await.stop().await?;
        self.strategy_router.write().await.stop().await?;
        self.executor.write().await.stop().await?;

        Ok(())
    }

    /// Get engine health status
    pub async fn health_check(&self) -> EngineHealth {
        let mempool_health = self.mempool_listener.read().await.health_check().await;
        let router_health = self.strategy_router.read().await.health_check().await;
        let executor_health = self.executor.read().await.health_check().await;

        EngineHealth {
            overall_healthy: mempool_health.healthy && router_health.healthy && executor_health.healthy,
            mempool: mempool_health,
            router: router_health,
            executor: executor_health,
        }
    }
}

