use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use std::time::{Duration, Instant};
use anchor_client::solana_sdk::pubkey::Pubkey;
use spl_token_2022::state::{Account, Mint};
use spl_token_2022::extension::StateWithExtensionsOwned;
use lazy_static::lazy_static;

/// TTL Cache entry that stores a value with an expiration time
pub struct CacheEntry<T> {
    pub value: T,
    pub expires_at: Instant,
}

impl<T> CacheEntry<T> {
    pub fn new(value: T, ttl_seconds: u64) -> Self {
        Self {
            value,
            expires_at: Instant::now() + Duration::from_secs(ttl_seconds),
        }
    }
    
    pub fn is_expired(&self) -> bool {
        Instant::now() > self.expires_at
    }
}

/// Token account cache
pub struct TokenAccountCache {
    accounts: RwLock<HashMap<Pubkey, CacheEntry<StateWithExtensionsOwned<Account>>>>,
    default_ttl: u64,
}

impl TokenAccountCache {
    pub fn new(default_ttl: u64) -> Self {
        Self {
            accounts: RwLock::new(HashMap::new()),
            default_ttl,
        }
    }
    
    pub fn get(&self, key: &Pubkey) -> Option<StateWithExtensionsOwned<Account>> {
        let accounts = self.accounts.read().unwrap();
        if let Some(entry) = accounts.get(key) {
            if !entry.is_expired() {
                return Some(entry.value.clone());
            }
        }
        None
    }
    
    pub fn insert(&self, key: Pubkey, value: StateWithExtensionsOwned<Account>, ttl: Option<u64>) {
        let ttl = ttl.unwrap_or(self.default_ttl);
        let mut accounts = self.accounts.write().unwrap();
        accounts.insert(key, CacheEntry::new(value, ttl));
    }
    
    pub fn remove(&self, key: &Pubkey) {
        let mut accounts = self.accounts.write().unwrap();
        accounts.remove(key);
    }
    
    pub fn clear_expired(&self) {
        let mut accounts = self.accounts.write().unwrap();
        accounts.retain(|_, entry| !entry.is_expired());
    }
    
    // Get the current size of the cache
    pub fn size(&self) -> usize {
        let accounts = self.accounts.read().unwrap();
        accounts.len()
    }
}

/// Token mint cache
pub struct TokenMintCache {
    mints: RwLock<HashMap<Pubkey, CacheEntry<StateWithExtensionsOwned<Mint>>>>,
    default_ttl: u64,
}

impl TokenMintCache {
    pub fn new(default_ttl: u64) -> Self {
        Self {
            mints: RwLock::new(HashMap::new()),
            default_ttl,
        }
    }
    
    pub fn get(&self, key: &Pubkey) -> Option<StateWithExtensionsOwned<Mint>> {
        let mints = self.mints.read().unwrap();
        if let Some(entry) = mints.get(key) {
            if !entry.is_expired() {
                return Some(entry.value.clone());
            }
        }
        None
    }
    
    pub fn insert(&self, key: Pubkey, value: StateWithExtensionsOwned<Mint>, ttl: Option<u64>) {
        let ttl = ttl.unwrap_or(self.default_ttl);
        let mut mints = self.mints.write().unwrap();
        mints.insert(key, CacheEntry::new(value, ttl));
    }
    
    pub fn remove(&self, key: &Pubkey) {
        let mut mints = self.mints.write().unwrap();
        mints.remove(key);
    }
    
    pub fn clear_expired(&self) {
        let mut mints = self.mints.write().unwrap();
        mints.retain(|_, entry| !entry.is_expired());
    }
    
    pub fn size(&self) -> usize {
        let mints = self.mints.read().unwrap();
        mints.len()
    }
}

/// Simple wallet token account tracker
pub struct WalletTokenAccounts {
    accounts: RwLock<HashSet<Pubkey>>,
}

impl WalletTokenAccounts {
    pub fn new() -> Self {
        Self {
            accounts: RwLock::new(HashSet::new()),
        }
    }
    
    pub fn contains(&self, account: &Pubkey) -> bool {
        let accounts = self.accounts.read().unwrap();
        accounts.contains(account)
    }
    
    pub fn insert(&self, account: Pubkey) -> bool {
        let mut accounts = self.accounts.write().unwrap();
        accounts.insert(account)
    }
    
    pub fn remove(&self, account: &Pubkey) -> bool {
        let mut accounts = self.accounts.write().unwrap();
        accounts.remove(account)
    }
    
    pub fn get_all(&self) -> HashSet<Pubkey> {
        let accounts = self.accounts.read().unwrap();
        accounts.clone()
    }
    
    pub fn clear(&self) {
        let mut accounts = self.accounts.write().unwrap();
        accounts.clear();
    }
    
    pub fn size(&self) -> usize {
        let accounts = self.accounts.read().unwrap();
        accounts.len()
    }
}

// Global cache instances with reasonable TTL values
lazy_static! {
    pub static ref TOKEN_ACCOUNT_CACHE: TokenAccountCache = TokenAccountCache::new(60); // 60 seconds TTL
    pub static ref TOKEN_MINT_CACHE: TokenMintCache = TokenMintCache::new(300); // 5 minutes TTL
    pub static ref WALLET_TOKEN_ACCOUNTS: WalletTokenAccounts = WalletTokenAccounts::new();
} 