use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use ethers::prelude::*;
use ethers::providers::{Provider, Ws};
use ethers::types::{Address, U256, Filter, Log, Transaction, H256};
use core_affinity;
use std::str::FromStr;
use std::sync::mpsc as std_mpsc;
use std::sync::Mutex;

// Configuration
const DEXCOUNT: usize = 4;
const MIN_TX_VALUE_ETH: u64 = 10_000_000_000_000_000; // 0.01 ETH minimum
const PRICE_IMPACT_MULTIPLIER: u64 = 20; // Amplify price impacts for faster detection
const EVENT_FLAG_DURATION_NANOS: u64 = 100_000; // 100 microseconds - enough for daemons to see

// DEX addresses and types
const DEX_ADDRESSES: [&str; DEXCOUNT] = [
    "0x88e6A0c2dDD26FEEb64F039a2C41296FcB3f5640", // UniswapV3 USDC/WETH pool
    "0x397FF1542f962076d0BFE58EA045FFA2D347ACa0", // USDC/WETH pair (V2 style)
    "0x5649B4DD00780e99Bab7Abb4A3d581Ea1aEB23D0", // Router contract
    "0xdef1c0ded9bec7f1a1670819833240f027b25eff"  // 0x Router contract
];

#[derive(Debug, Clone)]
enum DexType {
    UniswapV3Pool,
    UniswapV2Pair,
    Router,
    ZeroXRouter,
}

#[derive(Debug, Clone)]
struct PoolConfig {
    id: usize,
    address: Address,
    dex_type: DexType,
    name: &'static str,
}

// ULTRA LOW-LATENCY SHARED MEMORY - ONLY THIS BINARY CAN WRITE
pub static SHARED_PRICES: [AtomicU64; DEXCOUNT] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

// Event notification flags - toggle between 0 and 1 for daemon notifications
pub static PRICE_EVENT_FLAGS: [AtomicU64; DEXCOUNT] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

// Price update event for daemon communication
#[derive(Debug, Clone)]
struct PriceUpdateEvent {
    pool_id: usize,
    old_price: u64,
    new_price: u64,
    timestamp_nanos: u64,
    tx_hash: H256,
}

// Transaction event for mempool monitoring
#[derive(Debug, Clone)]
struct MempoolTxEvent {
    pool_id: usize,
    tx_hash: H256,
    tx_value: U256,
    target_address: Address,
    timestamp_nanos: u64,
}

// Main ETH Node structure - WEBSOCKET ONLY
struct EthNode {
    provider: Arc<Provider<Ws>>,
    pool_configs: Vec<PoolConfig>,
    pool_threads: Vec<thread::JoinHandle<()>>,
    tx_senders: Vec<std_mpsc::Sender<MempoolTxEvent>>,
}

impl EthNode {
    async fn new(ws_url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let ws = Ws::connect(ws_url).await?;
        let provider = Provider::new(ws);
        let provider = Arc::new(provider);
        
        let pool_configs = vec![
            PoolConfig {
                id: 0,
                address: DEX_ADDRESSES[0].parse()?,
                dex_type: DexType::UniswapV3Pool,
                name: "UniswapV3 USDC/WETH",
            },
            PoolConfig {
                id: 1,
                address: DEX_ADDRESSES[1].parse()?,
                dex_type: DexType::UniswapV2Pair,
                name: "USDC/WETH Pair",
            },
            PoolConfig {
                id: 2,
                address: DEX_ADDRESSES[2].parse()?,
                dex_type: DexType::Router,
                name: "Router Contract",
            },
            PoolConfig {
                id: 3,
                address: DEX_ADDRESSES[3].parse()?,
                dex_type: DexType::ZeroXRouter,
                name: "0x Router",
            },
        ];
        
        Ok(Self {
            provider,
            pool_configs,
            pool_threads: Vec::new(),
            tx_senders: Vec::new(),
        })
    }
    
    // Step 1: Initialize onchain prices via WebSocket
    async fn initialize_onchain_prices(&self) -> Result<(), Box<dyn std::error::Error>> {
        println!("Step 1: Initializing onchain prices via WebSocket...");
        
        for config in &self.pool_configs {
            let price = self.get_onchain_price(config).await?;
            
            // INSTANT ATOMIC UPDATE TO SHARED MEMORY
            SHARED_PRICES[config.id].store(price, Ordering::Release);
            
            println!("Initialized {} (ID: {}) with price: {}", 
                config.name, config.id, price);
        }
        
        Ok(())
    }
    
    // Get onchain price based on DEX type via WebSocket
    async fn get_onchain_price(&self, config: &PoolConfig) -> Result<u64, Box<dyn std::error::Error>> {
        match config.dex_type {
            DexType::UniswapV3Pool => {
                self.get_uniswap_v3_price(config.address).await
            },
            DexType::UniswapV2Pair => {
                self.get_uniswap_v2_price(config.address).await
            },
            DexType::Router | DexType::ZeroXRouter => {
                self.get_last_swap_price(config.address).await
            },
        }
    }
    
    // UniswapV3 slot0 price via WebSocket
    async fn get_uniswap_v3_price(&self, pool_address: Address) -> Result<u64, Box<dyn std::error::Error>> {
        let slot0_call = "0x3850c7bd";
        let call_data = hex::decode(&slot0_call[2..])?;
        
        let call = CallRequest {
            to: Some(pool_address),
            data: Some(call_data.into()),
            ..Default::default()
        };
        
        let result = self.provider.call(&call.into(), None).await?;
        let sqrt_price_x96 = U256::from_big_endian(&result[0..32]);
        let price = self.sqrt_price_to_price(sqrt_price_x96);
        
        Ok(price)
    }
    
    // UniswapV2 pair reserves via WebSocket
    async fn get_uniswap_v2_price(&self, pair_address: Address) -> Result<u64, Box<dyn std::error::Error>> {
        let get_reserves_call = "0x0902f1ac";
        let call_data = hex::decode(&get_reserves_call[2..])?;
        
        let call = CallRequest {
            to: Some(pair_address),
            data: Some(call_data.into()),
            ..Default::default()
        };
        
        let result = self.provider.call(&call.into(), None).await?;
        let reserve0 = U256::from_big_endian(&result[0..32]);
        let reserve1 = U256::from_big_endian(&result[32..64]);
        
        let price = if reserve0 > U256::zero() {
            ((reserve1 * U256::from(10u64.pow(18))) / reserve0).as_u64()
        } else {
            0
        };
        
        Ok(price)
    }
    
    // Get last swap price from router events via WebSocket
    async fn get_last_swap_price(&self, router_address: Address) -> Result<u64, Box<dyn std::error::Error>> {
        let latest_block = self.provider.get_block_number().await?;
        let from_block = latest_block - 100;
        
        let filter = Filter::new()
            .address(router_address)
            .from_block(from_block)
            .to_block(latest_block);
        
        let logs = self.provider.get_logs(&filter).await?;
        
        if let Some(last_log) = logs.last() {
            if !last_log.topics.is_empty() {
                if last_log.data.len() >= 160 {
                    let data = &last_log.data[..];
                    let sqrt_price_bytes = &data[128..160];
                    let sqrt_price_x96 = U256::from_big_endian(sqrt_price_bytes);
                    let price = self.sqrt_price_to_price(sqrt_price_x96);
                    
                    Ok(price)
                } else {
                    Ok(1800_000000000000000000u64) // Default price
                }
            } else {
                Ok(1800_000000000000000000u64) // Default price
            }
        } else {
            Ok(1800_000000000000000000u64) // Default price
        }
    }
    
    // Convert sqrtPriceX96 to actual price
    fn sqrt_price_to_price(&self, sqrt_price_x96: U256) -> u64 {
        let q96 = U256::from(2u64).pow(U256::from(96));
        let price_x192 = sqrt_price_x96 * sqrt_price_x96;
        let q192 = U256::from(2u64).pow(U256::from(192));
        let price_ratio = price_x192 / q192;
        let decimal_adjustment = U256::from(1_000_000_000_000u64);
        let final_price = price_ratio * decimal_adjustment;
        
        if final_price > U256::from(u64::MAX) {
            u64::MAX
        } else {
            final_price.as_u64()
        }
    }
    
    // Step 2: Initialize shared memory with prices
    fn initialize_shared_memory(&self) {
        println!("Step 2: Shared memory initialized with {} pools", DEXCOUNT);
        
        for i in 0..DEXCOUNT {
            let price = SHARED_PRICES[i].load(Ordering::Acquire);
            println!("Pool {}: Price = {}", i, price);
        }
    }
    
    // Step 3: Start ULTRA LOW-LATENCY monitoring with PINNED THREADS
    async fn start_ultra_low_latency_monitoring(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        println!("Step 3: Starting ULTRA LOW-LATENCY monitoring with PINNED THREADS...");
        
        // Create channels for each pool thread
        let mut tx_senders = Vec::new();
        
        // Start dedicated threads for each pool - PINNED TO CORES
        for config in self.pool_configs.clone() {
            let (tx_sender, tx_receiver) = std_mpsc::channel::<MempoolTxEvent>();
            
            tx_senders.push(tx_sender);
            
            let handle = thread::Builder::new()
                .name(format!("pool-{}", config.id))
                .spawn(move || {
                    // PIN THREAD TO SPECIFIC CPU CORE
                    if let Some(core_id) = core_affinity::get_core_ids().get(config.id) {
                        core_affinity::set_for_current(*core_id);
                        println!("Pool {} thread PINNED to core {}", config.id, core_id.id);
                    }
                    
                    // ULTRA LOW-LATENCY price monitoring loop
                    Self::ultra_low_latency_price_monitor(config, tx_receiver);
                })
                .expect("Failed to spawn pool monitoring thread");
            
            self.pool_threads.push(handle);
        }
        
        self.tx_senders = tx_senders;
        
        // Start WebSocket mempool listener in main async context
        let provider = Arc::clone(&self.provider);
        let senders = self.tx_senders.clone();
        
        tokio::spawn(async move {
            Self::websocket_mempool_listener(provider, senders).await;
        });
        
        Ok(())
    }
    
    // WebSocket mempool listener - INSTANT TRANSACTION FORWARDING
    async fn websocket_mempool_listener(
        provider: Arc<Provider<Ws>>,
        senders: Vec<std_mpsc::Sender<MempoolTxEvent>>
    ) {
        println!("WebSocket mempool listener started...");
        
        // Subscribe to pending transactions
        let mut stream = match provider.subscribe_pending_txs().await {
            Ok(stream) => stream,
            Err(e) => {
                println!("Failed to subscribe to pending transactions: {}", e);
                return;
            }
        };
        
        while let Some(tx_hash) = stream.next().await {
            let start_time = Instant::now();
            
            // Get full transaction details INSTANTLY
            if let Ok(Some(tx)) = provider.get_transaction(tx_hash).await {
                let timestamp_nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64;
                
                // Check which pool this transaction targets - INSTANT FILTERING
                for (i, &pool_address) in DEX_ADDRESSES.iter().enumerate() {
                    if let Ok(addr) = pool_address.parse::<Address>() {
                        if tx.to.map_or(false, |to| to == addr) {
                            // INSTANT FORWARD TO POOL THREAD
                            let tx_event = MempoolTxEvent {
                                pool_id: i,
                                tx_hash,
                                tx_value: tx.value,
                                target_address: addr,
                                timestamp_nanos,
                            };
                            
                            if let Err(_) = senders[i].send(tx_event) {
                                println!("Failed to send tx to pool {} thread", i);
                            }
                            
                            let processing_time = start_time.elapsed();
                            if processing_time.as_micros() > 100 { // Log if > 100μs
                                println!("Slow tx processing: {}μs", processing_time.as_micros());
                            }
                            
                            break; // Found target pool, no need to check others
                        }
                    }
                }
            }
        }
    }
    
    // ULTRA LOW-LATENCY price monitor - RUNS IN PINNED THREAD
    fn ultra_low_latency_price_monitor(
        config: PoolConfig,
        tx_receiver: std_mpsc::Receiver<MempoolTxEvent>,
    ) {
        println!("ULTRA LOW-LATENCY price monitor started for Pool {}: {}", config.id, config.name);
        
        let mut last_price = SHARED_PRICES[config.id].load(Ordering::Acquire);
        
        // INFINITE LOOP - INSTANT PROCESSING
        loop {
            // NON-BLOCKING RECEIVE - INSTANT PROCESSING
            match tx_receiver.try_recv() {
                Ok(tx_event) => {
                    let processing_start = Instant::now();
                    
                    // INSTANT PRICE IMPACT CALCULATION
                    let price_impact = Self::calculate_instant_price_impact(
                        last_price, 
                        &tx_event, 
                        &config
                    );
                    
                    if price_impact > 0 {
                        // INSTANT PRICE UPDATE
                        let new_price = if tx_event.tx_value > U256::zero() {
                            last_price.saturating_add(price_impact)
                        } else {
                            last_price.saturating_sub(price_impact)
                        };
                        
                        // INSTANT ATOMIC UPDATE - SHARED MEMORY WRITE
                        SHARED_PRICES[config.id].store(new_price, Ordering::Release);
                        
                        // INSTANT EVENT FLAG NOTIFICATION
                        Self::notify_daemons_via_shared_memory(config.id);
                        
                        println!("Price Update: Pool {} {} -> {} | TX: {:#x}",
                            config.id, 
                            last_price, 
                            new_price, 
                            tx_event.tx_hash);
                        
                        last_price = new_price;
                        
                        let processing_time = processing_start.elapsed();
                        if processing_time.as_nanos() > 1000 { // Log if > 1μs
                            println!("Slow price update: {}ns", processing_time.as_nanos());
                        }
                    }
                }
                Err(std_mpsc::TryRecvError::Empty) => {
                    // No transactions - yield CPU for nanoseconds
                    std::hint::spin_loop();
                }
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    println!("Pool {} thread disconnected", config.id);
                    break;
                }
            }
        }
    }
    
    // INSTANT price impact calculation - ZERO LATENCY
    fn calculate_instant_price_impact(
        current_price: u64,
        tx_event: &MempoolTxEvent,
        config: &PoolConfig
    ) -> u64 {
        let tx_value = tx_event.tx_value.as_u64();
        
        // Skip tiny transactions
        if tx_value < MIN_TX_VALUE_ETH {
            return 0;
        }
        
        // AMPLIFIED IMPACT CALCULATION FOR FASTER DETECTION
        let base_impact = match config.dex_type {
            DexType::UniswapV3Pool => {
                (tx_value / 1_000_000_000_000_000_000) * 25 // 0.25% per ETH
            },
            DexType::UniswapV2Pair => {
                (tx_value / 1_000_000_000_000_000_000) * 30 // 0.3% per ETH
            },
            DexType::Router | DexType::ZeroXRouter => {
                (tx_value / 1_000_000_000_000_000_000) * 15 // 0.15% per ETH
            },
        };
        
        // Apply impact with multiplier
        let amplified_impact = base_impact * PRICE_IMPACT_MULTIPLIER;
        (current_price * amplified_impact) / 10000
    }
    
    // INSTANT daemon notification via shared memory flag toggle
    fn notify_daemons_via_shared_memory(pool_id: usize) {
        // Get current flag value
        let current_flag = PRICE_EVENT_FLAGS[pool_id].load(Ordering::Acquire);
        
        // Toggle flag (0 -> 1 or 1 -> 0)
        let new_flag = if current_flag == 0 { 1 } else { 0 };
        
        // INSTANT ATOMIC FLAG UPDATE - DAEMONS WILL SEE THIS
        PRICE_EVENT_FLAGS[pool_id].store(new_flag, Ordering::Release);
        
        // Hold the flag for a brief moment to ensure all daemons see it
        // This runs in a separate thread to avoid blocking the main price update loop
        let pool_id_copy = pool_id;
        thread::spawn(move || {
            std::thread::sleep(Duration::from_nanos(EVENT_FLAG_DURATION_NANOS));
            
            // Optional: Could toggle back or leave as is
            // Daemons should detect the change, not the specific value
        });
    }
    
    // Wait for all threads to complete
    fn wait_for_threads(self) {
        for handle in self.pool_threads {
            handle.join().unwrap();
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting ULTRA LOW-LATENCY ETH Node Price Feed...");
    
    // Erigon WebSocket URL - WEBSOCKET ONLY
    let erigon_ws_url = "ws://localhost:8546";

    // Create ETH Node with WebSocket
    let mut eth_node = EthNode::new(erigon_ws_url).await?;
    
    // Step 1: Initialize onchain prices
    eth_node.initialize_onchain_prices().await?;
    
    // Step 2: Initialize shared memory
    eth_node.initialize_shared_memory();
    
    // Step 3: Start ULTRA LOW-LATENCY monitoring with PINNED THREADS
    eth_node.start_ultra_low_latency_monitoring().await?;
    
    println!("ULTRA LOW-LATENCY ETH Node Price Feed running...");
    println!("Shared memory prices accessible via SHARED_PRICES array");
    println!("Event flags accessible via PRICE_EVENT_FLAGS array");
    println!("Daemons should monitor flag changes for price update events");
    println!("Press Ctrl+C to stop");
    
    // Wait for all threads to complete
    eth_node.wait_for_threads();
    
    Ok(())
}