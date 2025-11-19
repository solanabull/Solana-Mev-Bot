use std::collections::VecDeque;
use dashmap::DashMap;
use lazy_static::lazy_static;

/// One slot of market data for a token
#[derive(Clone, Debug)]
pub struct SlotSample {
    pub slot: u64,
    pub price: f64,
    pub buy_volume: f64,   // volume in SOL
    pub sell_volume: f64,  // volume in SOL
}

#[derive(Clone, Debug)]
pub struct TokenTimeseries {
    samples: VecDeque<SlotSample>,
    capacity: usize,
}

impl TokenTimeseries {
    pub fn new(capacity: usize) -> Self {
        Self { samples: VecDeque::with_capacity(capacity), capacity }
    }

    pub fn update(&mut self, slot: u64, price: f64, is_buy: bool, sol_volume: f64) {
        // Append or aggregate by slot
        if let Some(back) = self.samples.back_mut() {
            if back.slot == slot {
                back.price = price;
                if is_buy { back.buy_volume += sol_volume; } else { back.sell_volume += sol_volume; }
                return;
            }
        }

        let mut sample = SlotSample { slot, price, buy_volume: 0.0, sell_volume: 0.0 };
        if is_buy { sample.buy_volume = sol_volume; } else { sample.sell_volume = sol_volume; }
        self.samples.push_back(sample);
        while self.samples.len() > self.capacity { self.samples.pop_front(); }
    }

    pub fn lowest_price(&self) -> Option<f64> {
        self.samples.iter().map(|s| s.price).fold(None, |acc, p| match acc {
            None => Some(p),
            Some(minp) => Some(minp.min(p)),
        })
    }

    pub fn highest_price(&self) -> Option<f64> {
        self.samples.iter().map(|s| s.price).fold(None, |acc, p| match acc {
            None => Some(p),
            Some(maxp) => Some(maxp.max(p)),
        })
    }

    /// Detect a potential bottom after a drop:
    /// - Price dropped by at least min_drop_pct from recent high
    /// - Last `stabilize_slots` slots show non-decreasing price
    /// - Last `stabilize_slots` sell volume average is down by sell_decline_pct vs prior window
    pub fn detect_bottom_after_drop(&self, min_drop_pct: f64, sell_decline_pct: f64, stabilize_slots: usize) -> BottomSignal {
        if self.samples.len() < stabilize_slots * 2 + 2 { return BottomSignal::no(); }

        let high = match self.highest_price() { Some(h) => h, None => return BottomSignal::no() };
        let low = match self.lowest_price() { Some(l) => l, None => return BottomSignal::no() };
        if high <= 0.0 { return BottomSignal::no(); }

        let drop_pct = (high - low) / high * 100.0;
        if drop_pct < min_drop_pct { return BottomSignal::no(); }

        // Check stabilization on last `stabilize_slots` samples
        let n = self.samples.len();
        let recent: Vec<&SlotSample> = self.samples
            .iter()
            .skip(n - stabilize_slots)
            .take(stabilize_slots)
            .collect();
        let prev: Vec<&SlotSample> = self.samples
            .iter()
            .skip(n - stabilize_slots * 2)
            .take(stabilize_slots)
            .collect();

        // Non-decreasing price condition (allow slight noise)
        let mut non_decreasing = true;
        for w in recent.as_slice().windows(2) {
            if w[1].price + (w[1].price * 0.001) < w[0].price { // 0.1% tolerance
                non_decreasing = false;
                break;
            }
        }
        if !non_decreasing { return BottomSignal::no(); }

        // Sell volume decline condition
        let recent_sell_avg = recent.iter().map(|s| s.sell_volume).sum::<f64>() / stabilize_slots as f64;
        let prev_sell_avg = prev.iter().map(|s| s.sell_volume).sum::<f64>() / stabilize_slots as f64;
        if prev_sell_avg <= 0.0 { return BottomSignal::no(); }
        let decline_pct = if prev_sell_avg > 0.0 { (prev_sell_avg - recent_sell_avg) / prev_sell_avg * 100.0 } else { 0.0 };
        if decline_pct < sell_decline_pct { return BottomSignal::no(); }

        BottomSignal { is_bottom: true, lowest_price: low, drop_pct }
    }
}

#[derive(Clone, Debug)]
pub struct BottomSignal {
    pub is_bottom: bool,
    pub lowest_price: f64,
    pub drop_pct: f64,
}

impl BottomSignal {
    pub fn no() -> Self { Self { is_bottom: false, lowest_price: 0.0, drop_pct: 0.0 } }
}

lazy_static! {
    pub static ref TOKEN_TIMESERIES: DashMap<String, TokenTimeseries> = DashMap::new();
}

pub fn update_for_mint(mint: &str, slot: u64, price: f64, is_buy: bool, sol_volume: f64) {
    let mut entry = TOKEN_TIMESERIES.entry(mint.to_string()).or_insert_with(|| TokenTimeseries::new(20));
    entry.update(slot, price, is_buy, sol_volume);
}

pub fn analyze_bottom(mint: &str, min_drop_pct: f64, sell_decline_pct: f64, stabilize_slots: usize) -> BottomSignal {
    if let Some(ts) = TOKEN_TIMESERIES.get(mint) {
        ts.detect_bottom_after_drop(min_drop_pct, sell_decline_pct, stabilize_slots)
    } else {
        BottomSignal::no()
    }
}


