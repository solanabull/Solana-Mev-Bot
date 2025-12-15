//! Strategy router for MEV opportunity processing
//!
//! Routes detected opportunities to appropriate strategies for evaluation
//! and execution.

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::utils::config::Config;
use crate::utils::types::{ExecutableOpportunity, SimulationData, ExecutionData, ComponentHealth};
use crate::strategies::{ArbitrageStrategy, SandwichStrategy, LiquidationStrategy};

/// Strategy router for coordinating MEV strategies
#[derive(Debug)]
pub struct StrategyRouter {
    config: Config,
    arbitrage_strategy: Option<Arc<RwLock<ArbitrageStrategy>>>,
    sandwich_strategy: Option<Arc<RwLock<SandwichStrategy>>>,
    liquidation_strategy: Option<Arc<RwLock<LiquidationStrategy>>>,
    running: Arc<RwLock<bool>>,
    processed_opportunities: Arc<RwLock<u64>>,
    successful_trades: Arc<RwLock<u64>>,
}

impl StrategyRouter {
    /// Create new strategy router
    pub async fn new(
        arbitrage_strategy: Option<Arc<RwLock<ArbitrageStrategy>>>,
        sandwich_strategy: Option<Arc<RwLock<SandwichStrategy>>>,
        liquidation_strategy: Option<Arc<RwLock<LiquidationStrategy>>>,
        config: Config,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            config,
            arbitrage_strategy,
            sandwich_strategy,
            liquidation_strategy,
            running: Arc::new(RwLock::new(false)),
            processed_opportunities: Arc::new(RwLock::new(0)),
            successful_trades: Arc::new(RwLock::new(0)),
        })
    }

    /// Process opportunities from mempool listener
    pub async fn process_opportunities(
        &mut self,
        mempool_listener: &Arc<RwLock<super::MempoolListener>>,
        simulator: &Arc<RwLock<super::SimulationEngine>>,
        executor: &Arc<RwLock<super::Executor>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        *self.running.write().await = true;

        let mut opportunity_receiver = mempool_listener.read().await.get_opportunity_receiver();

        loop {
            if !*self.running.read().await {
                break;
            }

            match opportunity_receiver.recv().await {
                Ok(transaction) => {
                    *self.processed_opportunities.write().await += 1;

                    // Route to appropriate strategies
                    self.route_opportunity(
                        transaction,
                        simulator,
                        executor,
                    ).await?;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!("Opportunity channel closed");
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    tracing::warn!("Opportunity receiver lagged, some opportunities may have been missed");
                    continue;
                }
            }
        }

        Ok(())
    }

    /// Route opportunity to appropriate strategy
    async fn route_opportunity(
        &self,
        transaction: super::MempoolTransaction,
        simulator: &Arc<RwLock<super::SimulationEngine>>,
        executor: &Arc<RwLock<super::Executor>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Analyze transaction to determine strategy type
        let opportunity_type = self.analyze_transaction(&transaction)?;

        match opportunity_type {
            OpportunityType::Arbitrage => {
                if let Some(strategy) = &self.arbitrage_strategy {
                    let mut strategy_lock = strategy.write().await;
                    if let Some(opportunity) = strategy_lock.analyze_opportunity(&transaction).await? {
                        self.execute_opportunity(opportunity, simulator, executor).await?;
                    }
                }
            }
            OpportunityType::Sandwich => {
                if let Some(strategy) = &self.sandwich_strategy {
                    let mut strategy_lock = strategy.write().await;
                    if let Some(opportunity) = strategy_lock.analyze_opportunity(&transaction).await? {
                        self.execute_opportunity(opportunity, simulator, executor).await?;
                    }
                }
            }
            OpportunityType::Liquidation => {
                if let Some(strategy) = &self.liquidation_strategy {
                    let mut strategy_lock = strategy.write().await;
                    if let Some(opportunity) = strategy_lock.analyze_opportunity(&transaction).await? {
                        self.execute_opportunity(opportunity, simulator, executor).await?;
                    }
                }
            }
            OpportunityType::Unknown => {
                // Skip unknown opportunities
            }
        }

        Ok(())
    }

    /// Analyze transaction to determine opportunity type
    fn analyze_transaction(&self, transaction: &super::MempoolTransaction) -> Result<OpportunityType, Box<dyn std::error::Error>> {
        // Check for DEX program interactions
        let dex_programs: std::collections::HashSet<_> = self.config.mempool.dex_programs.iter()
            .filter_map(|p| p.parse::<solana_sdk::pubkey::Pubkey>().ok())
            .collect();

        let has_dex_interaction = transaction.instructions.iter()
            .any(|instr| dex_programs.contains(&instr.program_id));

        if !has_dex_interaction {
            return Ok(OpportunityType::Unknown);
        }

        // Analyze instruction patterns to determine strategy type
        for instruction in &transaction.instructions {
            if let Some(decoded) = &instruction.decoded_instruction {
                match decoded {
                    super::DecodedInstruction::Swap(swap) => {
                        // Check if this could be part of an arbitrage
                        if self.is_arbitrage_opportunity(swap) {
                            return Ok(OpportunityType::Arbitrage);
                        }
                        // Check if this could be sandwiched
                        if self.is_sandwich_opportunity(swap) {
                            return Ok(OpportunityType::Sandwich);
                        }
                    }
                    super::DecodedInstruction::Transfer(_) => {
                        // Check for liquidation patterns
                        if self.is_liquidation_opportunity(instruction) {
                            return Ok(OpportunityType::Liquidation);
                        }
                    }
                }
            }
        }

        Ok(OpportunityType::Unknown)
    }

    /// Check if transaction represents arbitrage opportunity
    fn is_arbitrage_opportunity(&self, swap: &super::SwapInstruction) -> bool {
        // Look for large swaps that might indicate price discrepancies
        // This is a simplified check - real implementation would analyze
        // cross-DEX prices and liquidity
        swap.amount_in > 1000000 // 1M lamports minimum
    }

    /// Check if transaction represents sandwich opportunity
    fn is_sandwich_opportunity(&self, swap: &super::SwapInstruction) -> bool {
        // Check if swap meets sandwich criteria
        let min_size = (self.config.sandwich.min_target_size_usd * 1000000.0) as u64; // Rough USD to lamports
        swap.amount_in >= min_size
    }

    /// Check if instruction represents liquidation opportunity
    fn is_liquidation_opportunity(&self, instruction: &super::InstructionData) -> bool {
        // Check if instruction interacts with lending protocols
        // This would check for specific liquidation-related instructions
        self.config.strategies.liquidation
    }

    /// Execute validated opportunity
    async fn execute_opportunity(
        &self,
        opportunity: Box<dyn ExecutableOpportunity>,
        simulator: &Arc<RwLock<super::SimulationEngine>>,
        executor: &Arc<RwLock<super::Executor>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Simulate opportunity
        let simulator_lock = simulator.read().await;
        let simulation_result = simulator_lock.simulate_opportunity(&*opportunity).await?;

        if !simulation_result.is_profitable {
            tracing::debug!("Opportunity not profitable after simulation");
            return Ok(());
        }

        // Execute opportunity
        let mut executor_lock = executor.write().await;
        let execution_result = executor_lock.execute_opportunity(&*opportunity).await?;

        if execution_result.success {
            *self.successful_trades.write().await += 1;
            tracing::info!("Successfully executed opportunity: {}", execution_result.signature);
        } else {
            tracing::warn!("Failed to execute opportunity: {}", execution_result.error);
        }

        Ok(())
    }

    /// Stop the strategy router
    pub async fn stop(&self) -> Result<(), Box<dyn std::error::Error>> {
        *self.running.write().await = false;
        Ok(())
    }

    /// Health check for monitoring
    pub async fn health_check(&self) -> super::ComponentHealth {
        let processed = *self.processed_opportunities.read().await;
        let successful = *self.successful_trades.read().await;

        super::ComponentHealth {
            healthy: true, // Router is always healthy if running
            last_active: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            error_count: 0, // Track this properly in production
            status_message: format!(
                "Processed {} opportunities, {} successful trades",
                processed, successful
            ),
        }
    }
}

/// Types of MEV opportunities
#[derive(Debug, Clone, PartialEq)]
enum OpportunityType {
    Arbitrage,
    Sandwich,
    Liquidation,
    Unknown,
}

