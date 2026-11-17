//! The numbers behind the status bar.
//!
//! A frontend renders these; it does not compute them. Keeping the arithmetic
//! here means the terminal bar and the GUI bar cannot disagree about the
//! context percentage or the token rate.

use std::time::{Duration, Instant};

use argent_ai::Usage;

/// Live counters for one session.
#[derive(Debug, Clone)]
pub struct Metrics {
    pub model: String,
    pub thinking: String,
    pub cwd: String,
    /// True between the request being sent and the run finishing.
    pub working: bool,
    /// Animation frame counter; a frontend may use it for a spinner.
    pub tick: u64,
    turn_started: Option<Instant>,
    last_turn: Option<Duration>,
    first_chunk: Option<Instant>,
    ttft: Option<Duration>,
    /// EMA-smoothed live rate: the raw reading jumps every frame, which
    /// makes the status bar unreadable while streaming.
    rate_ema: f64,
    rate_at: Option<Instant>,
    /// Characters received this turn, split by script, for a live estimate.
    ascii_chars: usize,
    cjk_chars: usize,
    other_chars: usize,
    /// Exact numbers from the provider, as of the last finished turn.
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub requests: u64,
    /// Estimated spend for the session, when prices are configured.
    pub cost: Option<f64>,
    /// Tokens the conversation occupies, as the next request would send them.
    pub context_used: usize,
    /// The model's window.
    pub context_max: usize,
    /// Session totals, accumulated turn by turn (the per-turn fields above
    /// only hold the latest turn).
    pub session_prompt_tokens: u64,
    pub session_completion_tokens: u64,
}

impl Metrics {
    pub fn new(
        model: impl Into<String>,
        thinking: impl Into<String>,
        cwd: impl Into<String>,
    ) -> Metrics {
        Metrics {
            model: model.into(),
            thinking: thinking.into(),
            cwd: cwd.into(),
            working: false,
            tick: 0,
            turn_started: None,
            last_turn: None,
            first_chunk: None,
            ttft: None,
            rate_ema: 0.0,
            rate_at: None,
            ascii_chars: 0,
            cjk_chars: 0,
            other_chars: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            requests: 0,
            cost: None,
            context_used: 0,
            context_max: 0,
            session_prompt_tokens: 0,
            session_completion_tokens: 0,
        }
    }

    /// A turn is starting.
    pub fn begin_turn(&mut self) {
        self.working = true;
        self.turn_started = Some(Instant::now());
        self.first_chunk = None;
        self.rate_ema = 0.0;
        self.rate_at = None;
        self.ttft = None;
        self.ascii_chars = 0;
        self.cjk_chars = 0;
        self.other_chars = 0;
    }

    /// Assistant text arrived; used for the live rate and the time-to-first-token.
    ///
    /// The chunk is classified by script rather than counted as bytes: a
    /// byte-based estimate is wrong in both directions, and for Chinese it is
    /// wrong by a factor of six.
    pub fn note_text(&mut self, chunk: &str) {
        if self.first_chunk.is_none() {
            self.first_chunk = Some(Instant::now());
            if let Some(start) = self.turn_started {
                self.ttft = Some(self.first_chunk.unwrap_or(start) - start);
            }
        }
        for ch in chunk.chars() {
            if ch.is_ascii() {
                self.ascii_chars += 1;
            } else if is_cjk(ch) {
                self.cjk_chars += 1;
            } else {
                self.other_chars += 1;
            }
        }
    }

    /// The turn finished with the provider's own numbers.
    pub fn finish_turn(&mut self, usage: Usage, requests: u64) {
        self.working = false;
        if let Some(start) = self.turn_started {
            self.last_turn = Some(start.elapsed());
        }
        self.prompt_tokens = usage.prompt_tokens;
        self.completion_tokens = usage.completion_tokens;
        self.cached_tokens = usage.cached_tokens;
        self.requests = requests;
        self.session_prompt_tokens += usage.prompt_tokens;
        self.session_completion_tokens += usage.completion_tokens;
    }

    /// Share of the prompt that the provider served from its cache.
    pub fn cache_hit_percent(&self) -> Option<u64> {
        let capped = self.cached_tokens.min(self.prompt_tokens);
        (self.prompt_tokens > 0).then(|| capped * 100 / self.prompt_tokens)
    }

    /// How long the current or last turn took.
    pub fn elapsed(&self) -> Duration {
        match (&self.turn_started, self.working) {
            (Some(start), true) => start.elapsed(),
            (_, _) => self.last_turn.unwrap_or_default(),
        }
    }

    pub fn ttft(&self) -> Option<Duration> {
        self.ttft
    }

    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        // 每个帧推进一次 EMA：τ=0.6s，比帧率平滑，但仍跟得上真实变化。
        let rate = self.live_rate();
        let now = Instant::now();
        let dt = self
            .rate_at
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(0.033);
        self.rate_at = Some(now);
        let alpha = dt / (dt + 0.6);
        self.rate_ema = if self.rate_ema == 0.0 {
            rate
        } else {
            self.rate_ema + alpha * (rate - self.rate_ema)
        };
    }

    /// The instantaneous rate, low-pass filtered; stable enough for a status bar.
    pub fn smoothed_live_rate(&self) -> f64 {
        self.rate_ema
    }

    /// Tokens received this turn.
    ///
    /// Exact once the provider has answered; before that, an estimate from the
    /// text that has actually arrived.
    pub fn tokens_down(&self) -> u64 {
        if !self.working {
            return self.completion_tokens;
        }
        let ascii = self.ascii_chars.div_ceil(4);
        let cjk = self.cjk_chars * 3 / 2;
        (ascii + cjk + self.other_chars) as u64
    }

    /// Tokens per second over the current turn.
    pub fn live_rate(&self) -> f64 {
        if !self.working {
            return 0.0;
        }
        let Some(start) = self.first_chunk else {
            return 0.0;
        };
        let seconds = start.elapsed().as_secs_f64();
        if seconds < 0.2 {
            return 0.0;
        }
        self.tokens_down() as f64 / seconds
    }

    /// Tokens per second over the whole session.
    pub fn average_rate(&self) -> f64 {
        let elapsed = self.elapsed().as_secs_f64();
        if elapsed < 0.2 {
            return 0.0;
        }
        self.completion_tokens as f64 / elapsed
    }

    /// The rate a status bar should show.
    pub fn rate(&self) -> f64 {
        if self.working {
            self.live_rate()
        } else {
            self.average_rate()
        }
    }

    /// How full the context window is, `0.0..=1.0`.
    pub fn context_fraction(&self) -> f32 {
        if self.context_max == 0 {
            return 0.0;
        }
        (self.context_used as f32 / self.context_max as f32).clamp(0.0, 1.0)
    }

    /// The share of prompt tokens the provider served from cache.
    pub fn cache_fraction(&self) -> f32 {
        if self.prompt_tokens == 0 {
            return 0.0;
        }
        (self.cached_tokens as f32 / self.prompt_tokens as f32).clamp(0.0, 1.0)
    }
}

/// True for the CJK ranges, which cost roughly 1.5 tokens per character.
fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{4e00}'..='\u{9fff}'
        | '\u{3400}'..='\u{4dbf}'
        | '\u{3040}'..='\u{30ff}'
        | '\u{ac00}'..='\u{d7af}'
        | '\u{f900}'..='\u{faff}')
}

/// `1.4s`, `2m 05s`, `1h 03m`.
pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{:.1}s", duration.as_secs_f64())
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// `8.2k`, `1.0M`, or the plain number when small.
pub fn format_tokens(tokens: u64) -> String {
    if tokens < 1000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        return format!("{:.1}k", tokens as f64 / 1000.0);
    }
    format!("{:.1}M", tokens as f64 / 1_000_000.0)
}

/// An ASCII bar for frontends that draw with characters.
pub fn progress_bar(fraction: f32, width: usize) -> String {
    let filled = ((fraction.clamp(0.0, 1.0)) * width as f32).round() as usize;
    let mut text = String::with_capacity(width);
    for index in 0..width {
        text.push(if index < filled { '█' } else { '░' });
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> Metrics {
        Metrics::new("m", "high", "/tmp")
    }

    #[test]
    fn a_new_session_has_no_numbers() {
        let m = metrics();
        assert_eq!(m.tokens_down(), 0);
        assert_eq!(m.context_fraction(), 0.0);
        assert_eq!(m.cache_fraction(), 0.0);
        assert_eq!(m.rate(), 0.0);
    }

    #[test]
    fn finishing_a_turn_records_the_provider_numbers() {
        let mut m = metrics();
        m.begin_turn();
        m.finish_turn(
            Usage {
                prompt_tokens: 100,
                completion_tokens: 40,
                total_tokens: 140,
                cached_tokens: 90,
            },
            2,
        );
        assert!(!m.working);
        assert_eq!(m.tokens_down(), 40);
        assert_eq!(m.prompt_tokens, 100);
        assert_eq!(m.requests, 2);
        assert!((m.cache_fraction() - 0.9).abs() < 0.001);
        m.begin_turn();
        m.finish_turn(
            Usage {
                prompt_tokens: 50,
                completion_tokens: 20,
                total_tokens: 70,
                cached_tokens: 0,
            },
            1,
        );
        assert_eq!(m.session_prompt_tokens, 150);
        assert_eq!(m.session_completion_tokens, 60);
    }

    #[test]
    fn the_context_bar_measures_against_the_window() {
        let mut m = metrics();
        m.context_max = 1000;
        m.context_used = 640;
        assert!((m.context_fraction() - 0.64).abs() < 0.001);
        m.context_used = 5000;
        assert_eq!(m.context_fraction(), 1.0);
    }

    #[test]
    fn a_window_of_zero_does_not_divide_by_zero() {
        let mut m = metrics();
        m.context_used = 100;
        assert_eq!(m.context_fraction(), 0.0);
    }

    #[test]
    fn ttft_needs_a_first_chunk() {
        let mut m = metrics();
        m.begin_turn();
        assert!(m.ttft().is_none());
        m.note_text("abcd");
        assert!(m.ttft().is_some());
    }

    #[test]
    fn live_tokens_are_an_estimate_while_working() {
        let mut m = metrics();
        m.begin_turn();
        m.note_text(&"a".repeat(400));
        assert_eq!(m.tokens_down(), 100);
    }

    #[test]
    fn chinese_is_counted_per_character_not_per_byte() {
        let mut m = metrics();
        m.begin_turn();
        m.note_text("中文测试");
        // Four CJK characters: six tokens, not one (which is what a byte count
        // divided by four would claim).
        assert_eq!(m.tokens_down(), 6);
    }

    #[test]
    fn the_counter_resets_when_a_turn_starts() {
        let mut m = metrics();
        m.begin_turn();
        m.note_text("abcd");
        assert_eq!(m.tokens_down(), 1);
        m.finish_turn(Usage::default(), 1);
        m.begin_turn();
        assert_eq!(m.tokens_down(), 0);
    }

    #[test]
    fn the_live_rate_is_smoothed_by_tick() {
        let mut m = metrics();
        m.begin_turn();
        m.note_text(&"a".repeat(400));
        // live_rate 只在有首个 chunk 200ms 之后报数；等一帧真时钟。
        std::thread::sleep(Duration::from_millis(250));
        for _ in 0..200 {
            m.note_text(&"a".repeat(400));
            m.tick();
        }
        assert!(m.smoothed_live_rate() > 0.0);
        // 平滑值不能超过瞬时值太多（同量级）。
        assert!(m.smoothed_live_rate() <= m.live_rate().max(f64::EPSILON) * 1.5 + 1.0);
    }

    #[test]
    fn formats_scale() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(8200), "8.2k");
        assert_eq!(format_tokens(1_000_000), "1.0M");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(Duration::from_millis(1400)), "1.4s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 05s");
        assert_eq!(format_duration(Duration::from_secs(3780)), "1h 03m");
    }

    #[test]
    fn the_bar_fills_proportionally() {
        assert_eq!(progress_bar(0.0, 4), "░░░░");
        assert_eq!(progress_bar(0.5, 4), "██░░");
        assert_eq!(progress_bar(1.0, 4), "████");
    }
}
