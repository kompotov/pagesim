//! pagesim — симулятор алгоритмів заміщення сторінок пам'яті.
//! Модель: процес звертається до віртуальних сторінок, ОС має обмежену
//! кількість фізичних кадрів. Програма підраховує сторінкові переривання
//! (page faults) для кожного алгоритму на однакових трасах звернень.

use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::process;
use std::time::Instant;

/// Одне звернення до пам'яті: номер віртуальної сторінки та ознака запису.
#[derive(Clone, Copy, Debug)]
struct Access {
    page: u32,
    write: bool,
}

/// Результат моделювання одного алгоритму на одній трасі.
#[derive(Debug, Default, Clone)]
struct Stats {
    faults: u64,
    hits: u64,
    writebacks: u64, // скільки «брудних» сторінок довелося записати на диск
}

impl Stats {
    fn fault_rate(&self) -> f64 {
        let total = self.faults + self.hits;
        if total == 0 { 0.0 } else { self.faults as f64 / total as f64 }
    }
}

/// Генератор псевдовипадкових чисел xorshift64* (детермінований, без залежностей).
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self { Rng(seed.max(1)) }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u32) -> u32 { (self.next_u64() % n as u64) as u32 }
    fn chance(&mut self, p: f64) -> bool { ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64) < p }
}

/// Спільний інтерфейс усіх алгоритмів заміщення.
#[allow(dead_code)]
trait Replacer {
    fn name(&self) -> &'static str;
    /// Обробити звернення з номером `t` у трасі. Повертає true, якщо був промах.
    fn access(&mut self, t: usize, a: Access, st: &mut Stats) -> bool;
}

/// Таблиця сторінок у спрощеному вигляді: сторінка -> індекс кадру.
struct Frames {
    cap: usize,
    page_of: Vec<Option<u32>>, // вміст кожного кадру
    frame_of: HashMap<u32, usize>,
    dirty: Vec<bool>,
}

impl Frames {
    fn new(cap: usize) -> Self {
        Frames { cap, page_of: Vec::with_capacity(cap), frame_of: HashMap::new(), dirty: Vec::with_capacity(cap) }
    }
    fn lookup(&self, page: u32) -> Option<usize> { self.frame_of.get(&page).copied() }
    fn is_full(&self) -> bool { self.page_of.len() >= self.cap }
    /// Завантажити сторінку у вільний кадр (поки пам'ять не заповнена).
    fn load_free(&mut self, a: Access) -> usize {
        let f = self.page_of.len();
        self.page_of.push(Some(a.page));
        self.dirty.push(a.write);
        self.frame_of.insert(a.page, f);
        f
    }
    /// Витіснити вміст кадру `f` і завантажити туди нову сторінку.
    fn replace(&mut self, f: usize, a: Access, st: &mut Stats) {
        if let Some(old) = self.page_of[f] {
            self.frame_of.remove(&old);
            if self.dirty[f] { st.writebacks += 1; }
        }
        self.page_of[f] = Some(a.page);
        self.dirty[f] = a.write;
        self.frame_of.insert(a.page, f);
    }
}

// ---------------------------------------------------------------- FIFO
struct Fifo { fr: Frames, queue: VecDeque<usize> }
impl Fifo { fn new(c: usize) -> Self { Fifo { fr: Frames::new(c), queue: VecDeque::new() } } }
impl Replacer for Fifo {
    fn name(&self) -> &'static str { "FIFO" }
    fn access(&mut self, _t: usize, a: Access, st: &mut Stats) -> bool {
        if let Some(f) = self.fr.lookup(a.page) {
            self.fr.dirty[f] |= a.write;
            st.hits += 1;
            return false;
        }
        st.faults += 1;
        if !self.fr.is_full() {
            let f = self.fr.load_free(a);
            self.queue.push_back(f);
        } else {
            let victim = self.queue.pop_front().expect("черга FIFO порожня");
            self.fr.replace(victim, a, st);
            self.queue.push_back(victim);
        }
        true
    }
}

// ---------------------------------------------------------------- LRU
// Точна реалізація: для кожного кадру зберігається час останнього звернення.
struct Lru { fr: Frames, last: Vec<usize> }
impl Lru { fn new(c: usize) -> Self { Lru { fr: Frames::new(c), last: Vec::new() } } }
impl Replacer for Lru {
    fn name(&self) -> &'static str { "LRU" }
    fn access(&mut self, t: usize, a: Access, st: &mut Stats) -> bool {
        if let Some(f) = self.fr.lookup(a.page) {
            self.fr.dirty[f] |= a.write;
            self.last[f] = t;
            st.hits += 1;
            return false;
        }
        st.faults += 1;
        if !self.fr.is_full() {
            self.fr.load_free(a);
            self.last.push(t);
        } else {
            let victim = (0..self.fr.cap).min_by_key(|&f| self.last[f]).unwrap();
            self.fr.replace(victim, a, st);
            self.last[victim] = t;
        }
        true
    }
}

// ---------------------------------------------------------------- OPT (алгоритм Біледі)
// Потребує знання майбутнього, тому отримує всю трасу наперед.
struct Opt { fr: Frames, next_use: Vec<usize> }
impl Opt {
    fn new(c: usize, trace: &[Access]) -> Self {
        // next_use[i] — позиція наступного звернення до тієї ж сторінки після i
        let mut next_use = vec![usize::MAX; trace.len()];
        let mut seen: HashMap<u32, usize> = HashMap::new();
        for i in (0..trace.len()).rev() {
            if let Some(&j) = seen.get(&trace[i].page) { next_use[i] = j; }
            seen.insert(trace[i].page, i);
        }
        Opt { fr: Frames::new(c), next_use }
    }
}
struct OptState { inner: Opt, next_of_frame: Vec<usize> }
impl Replacer for OptState {
    fn name(&self) -> &'static str { "OPT" }
    fn access(&mut self, t: usize, a: Access, st: &mut Stats) -> bool {
        let nu = self.inner.next_use[t];
        if let Some(f) = self.inner.fr.lookup(a.page) {
            self.inner.fr.dirty[f] |= a.write;
            self.next_of_frame[f] = nu;
            st.hits += 1;
            return false;
        }
        st.faults += 1;
        if !self.inner.fr.is_full() {
            self.inner.fr.load_free(a);
            self.next_of_frame.push(nu);
        } else {
            let victim = (0..self.inner.fr.cap).max_by_key(|&f| self.next_of_frame[f]).unwrap();
            self.inner.fr.replace(victim, a, st);
            self.next_of_frame[victim] = nu;
        }
        true
    }
}

// ---------------------------------------------------------------- CLOCK (друга спроба)
struct Clock { fr: Frames, refbit: Vec<bool>, hand: usize }
impl Clock { fn new(c: usize) -> Self { Clock { fr: Frames::new(c), refbit: Vec::new(), hand: 0 } } }
impl Replacer for Clock {
    fn name(&self) -> &'static str { "CLOCK" }
    fn access(&mut self, _t: usize, a: Access, st: &mut Stats) -> bool {
        if let Some(f) = self.fr.lookup(a.page) {
            self.fr.dirty[f] |= a.write;
            self.refbit[f] = true;
            st.hits += 1;
            return false;
        }
        st.faults += 1;
        if !self.fr.is_full() {
            self.fr.load_free(a);
            self.refbit.push(true);
            return true;
        }
        // стрілка рухається по колу, знімаючи біти звернення, доки не знайде R = 0
        loop {
            if self.refbit[self.hand] {
                self.refbit[self.hand] = false;
                self.hand = (self.hand + 1) % self.fr.cap;
            } else {
                let victim = self.hand;
                self.fr.replace(victim, a, st);
                self.refbit[victim] = true;
                self.hand = (self.hand + 1) % self.fr.cap;
                return true;
            }
        }
    }
}

// ---------------------------------------------------------------- NRU
// Біти R скидаються «таймером» кожні `period` звернень; жертва — кадр
// з найнижчого непорожнього класу (R, M), обраний псевдовипадково.
struct Nru { fr: Frames, refbit: Vec<bool>, period: usize, rng: Rng }
impl Nru { fn new(c: usize, period: usize) -> Self { Nru { fr: Frames::new(c), refbit: Vec::new(), period, rng: Rng::new(7) } } }
impl Replacer for Nru {
    fn name(&self) -> &'static str { "NRU" }
    fn access(&mut self, t: usize, a: Access, st: &mut Stats) -> bool {
        if t % self.period == 0 { self.refbit.iter_mut().for_each(|r| *r = false); }
        if let Some(f) = self.fr.lookup(a.page) {
            self.fr.dirty[f] |= a.write;
            self.refbit[f] = true;
            st.hits += 1;
            return false;
        }
        st.faults += 1;
        if !self.fr.is_full() {
            self.fr.load_free(a);
            self.refbit.push(true);
            return true;
        }
        let class = |f: usize, s: &Nru| (s.refbit[f] as u8) * 2 + s.fr.dirty[f] as u8;
        let min_class = (0..self.fr.cap).map(|f| class(f, self)).min().unwrap();
        let cands: Vec<usize> = (0..self.fr.cap).filter(|&f| class(f, self) == min_class).collect();
        let victim = cands[self.rng.below(cands.len() as u32) as usize];
        self.fr.replace(victim, a, st);
        self.refbit[victim] = true;
        true
    }
}

// ---------------------------------------------------------------- LFU
struct Lfu { fr: Frames, count: Vec<u64>, loaded: Vec<usize> }
impl Lfu { fn new(c: usize) -> Self { Lfu { fr: Frames::new(c), count: Vec::new(), loaded: Vec::new() } } }
impl Replacer for Lfu {
    fn name(&self) -> &'static str { "LFU" }
    fn access(&mut self, t: usize, a: Access, st: &mut Stats) -> bool {
        if let Some(f) = self.fr.lookup(a.page) {
            self.fr.dirty[f] |= a.write;
            self.count[f] += 1;
            st.hits += 1;
            return false;
        }
        st.faults += 1;
        if !self.fr.is_full() {
            self.fr.load_free(a);
            self.count.push(1);
            self.loaded.push(t);
        } else {
            // за рівних лічильників витісняється сторінка, завантажена раніше
            let victim = (0..self.fr.cap).min_by_key(|&f| (self.count[f], self.loaded[f])).unwrap();
            self.fr.replace(victim, a, st);
            self.count[victim] = 1;
            self.loaded[victim] = t;
        }
        true
    }
}

// ---------------------------------------------------------------- AGING
// 8-бітний програмний лічильник: кожен «тік» зсув праворуч і запис R у старший біт.
struct Aging { fr: Frames, age: Vec<u8>, refbit: Vec<bool>, period: usize }
impl Aging { fn new(c: usize, period: usize) -> Self { Aging { fr: Frames::new(c), age: Vec::new(), refbit: Vec::new(), period } } }
impl Replacer for Aging {
    fn name(&self) -> &'static str { "AGING" }
    fn access(&mut self, t: usize, a: Access, st: &mut Stats) -> bool {
        if t % self.period == 0 {
            for f in 0..self.age.len() {
                self.age[f] = (self.age[f] >> 1) | if self.refbit[f] { 0x80 } else { 0 };
                self.refbit[f] = false;
            }
        }
        if let Some(f) = self.fr.lookup(a.page) {
            self.fr.dirty[f] |= a.write;
            self.refbit[f] = true;
            st.hits += 1;
            return false;
        }
        st.faults += 1;
        if !self.fr.is_full() {
            self.fr.load_free(a);
            self.age.push(0);
            self.refbit.push(true);
        } else {
            let victim = (0..self.fr.cap).min_by_key(|&f| (self.refbit[f], self.age[f])).unwrap();
            self.fr.replace(victim, a, st);
            self.age[victim] = 0;
            self.refbit[victim] = true;
        }
        true
    }
}

// ---------------------------------------------------------------- RANDOM
struct Random { fr: Frames, rng: Rng }
impl Random { fn new(c: usize) -> Self { Random { fr: Frames::new(c), rng: Rng::new(2026) } } }
impl Replacer for Random {
    fn name(&self) -> &'static str { "RANDOM" }
    fn access(&mut self, _t: usize, a: Access, st: &mut Stats) -> bool {
        if let Some(f) = self.fr.lookup(a.page) {
            self.fr.dirty[f] |= a.write;
            st.hits += 1;
            return false;
        }
        st.faults += 1;
        if !self.fr.is_full() { self.fr.load_free(a); } else {
            let victim = self.rng.below(self.fr.cap as u32) as usize;
            self.fr.replace(victim, a, st);
        }
        true
    }
}

const ALGS: [&str; 8] = ["OPT", "FIFO", "LRU", "CLOCK", "NRU", "AGING", "LFU", "RANDOM"];
const TICK: usize = 50; // період «таймера» для NRU та AGING (у зверненнях)

fn make(alg: &str, frames: usize, trace: &[Access]) -> Result<Box<dyn Replacer>, String> {
    if frames == 0 { return Err("кількість кадрів має бути більшою за нуль".into()); }
    Ok(match alg.to_uppercase().as_str() {
        "FIFO" => Box::new(Fifo::new(frames)),
        "LRU" => Box::new(Lru::new(frames)),
        "OPT" => Box::new(OptState { inner: Opt::new(frames, trace), next_of_frame: Vec::new() }),
        "CLOCK" => Box::new(Clock::new(frames)),
        "NRU" => Box::new(Nru::new(frames, TICK)),
        "AGING" => Box::new(Aging::new(frames, TICK)),
        "LFU" => Box::new(Lfu::new(frames)),
        "RANDOM" => Box::new(Random::new(frames)),
        other => return Err(format!("невідомий алгоритм: {other}")),
    })
}

fn simulate(alg: &str, frames: usize, trace: &[Access]) -> Result<(Stats, f64), String> {
    let mut r = make(alg, frames, trace)?;
    let mut st = Stats::default();
    let start = Instant::now();
    for (t, &a) in trace.iter().enumerate() { r.access(t, a, &mut st); }
    let ns = start.elapsed().as_nanos() as f64 / trace.len().max(1) as f64;
    Ok((st, ns))
}

// ---------------------------------------------------------------- генератори трас
fn gen_uniform(n: usize, pages: u32, rng: &mut Rng) -> Vec<Access> {
    (0..n).map(|_| Access { page: rng.below(pages), write: rng.chance(0.3) }).collect()
}

/// Фазова локальність: програма довго працює з невеликою робочою множиною,
/// потім переходить до іншої. 90 % звернень — усередині поточної множини.
fn gen_locality(n: usize, pages: u32, ws: u32, phase: usize, rng: &mut Rng) -> Vec<Access> {
    let mut v = Vec::with_capacity(n);
    let mut base = 0;
    for i in 0..n {
        if i % phase == 0 { base = rng.below(pages - ws); }
        let page = if rng.chance(0.9) { base + rng.below(ws) } else { rng.below(pages) };
        v.push(Access { page, write: rng.chance(0.3) });
    }
    v
}

/// Циклічний прохід по масиву, що трохи більший за пам'ять.
fn gen_loop(n: usize, pages: u32) -> Vec<Access> {
    (0..n).map(|i| Access { page: (i as u32) % pages, write: false }).collect()
}

/// «Гаряча» множина: 80 % звернень припадає на 20 % сторінок.
fn gen_hotcold(n: usize, pages: u32, rng: &mut Rng) -> Vec<Access> {
    let hot = pages / 5;
    (0..n).map(|_| {
        let page = if rng.chance(0.8) { rng.below(hot) } else { hot + rng.below(pages - hot) };
        Access { page, write: rng.chance(0.3) }
    }).collect()
}

/// Гаряча множина, яку періодично «забруднює» послідовне сканування.
fn gen_scan_mix(n: usize, rng: &mut Rng) -> Vec<Access> {
    let mut v = Vec::with_capacity(n);
    let mut scan_pos = 1000u32;
    while v.len() < n {
        for _ in 0..400 { v.push(Access { page: rng.below(24), write: rng.chance(0.3) }); }
        for _ in 0..200 { v.push(Access { page: scan_pos, write: false }); scan_pos += 1; }
    }
    v.truncate(n);
    v
}

fn workloads() -> Vec<(&'static str, Vec<Access>)> {
    let n = 100_000;
    vec![
        ("uniform", gen_uniform(n, 100, &mut Rng::new(11))),
        ("locality", gen_locality(n, 200, 12, 2_000, &mut Rng::new(12))),
        ("loop", gen_loop(n, 40)),
        ("hotcold", gen_hotcold(n, 200, &mut Rng::new(13))),
        ("scanmix", gen_scan_mix(n, &mut Rng::new(14))),
    ]
}

fn parse_trace(text: &str) -> Result<Vec<Access>, String> {
    let mut v = Vec::new();
    for (ln, tok) in text.split(|c: char| c.is_whitespace() || c == ',').filter(|s| !s.is_empty()).enumerate() {
        let (num, write) = match tok.strip_suffix(['w', 'W']) { Some(s) => (s, true), None => (tok, false) };
        let page = num.parse::<u32>().map_err(|_| format!("елемент {}: «{tok}» не є номером сторінки", ln + 1))?;
        v.push(Access { page, write });
    }
    if v.is_empty() { return Err("траса звернень порожня".into()); }
    Ok(v)
}

fn usage() -> ! {
    eprintln!("Використання:\n  pagesim run <файл_траси> <кадри> [алгоритм|all]\n  pagesim string \"1 2 3 4 1 2 5\" <кадри> [алгоритм|all]\n  pagesim belady\n  pagesim bench <вихідний.csv>");
    process::exit(2);
}

fn print_table(trace: &[Access], frames: usize, algs: &[&str]) -> Result<(), String> {
    println!("{:<8} {:>10} {:>10} {:>9} {:>11}", "Алгоритм", "Промахи", "Влучання", "Частка,%", "Записи");
    for a in algs {
        let (st, _) = simulate(a, frames, trace)?;
        println!("{:<8} {:>10} {:>10} {:>9.2} {:>11}", a, st.faults, st.hits, st.fault_rate() * 100.0, st.writebacks);
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 { usage(); }
    let pick = |i: usize| -> Vec<&str> {
        match args.get(i).map(|s| s.as_str()) {
            None | Some("all") => ALGS.to_vec(),
            Some(s) => vec![ALGS.iter().copied().find(|a| a.eq_ignore_ascii_case(s)).unwrap_or("?")],
        }
    };
    match args[1].as_str() {
        "run" | "string" => {
            if args.len() < 4 { usage(); }
            let text = if args[1] == "run" {
                fs::read_to_string(&args[2]).map_err(|e| format!("не вдалося прочитати {}: {e}", args[2]))?
            } else { args[2].clone() };
            let trace = parse_trace(&text)?;
            let frames: usize = args[3].parse().map_err(|_| format!("«{}» не є кількістю кадрів", args[3]))?;
            print_table(&trace, frames, &pick(4))
        }
        "belady" => {
            let trace = parse_trace("1 2 3 4 1 2 5 1 2 3 4 5")?;
            for alg in ["FIFO", "LRU", "OPT"] {
                let row: Vec<String> = (1..=6).map(|f| simulate(alg, f, &trace).map(|r| r.0.faults.to_string())).collect::<Result<_, _>>()?;
                println!("{alg:<5} кадри 1..6: {}", row.join(" "));
            }
            Ok(())
        }
        "bench" => {
            let out = args.get(2).ok_or("вкажіть вихідний CSV-файл")?;
            let mut f = fs::File::create(out).map_err(|e| format!("не вдалося створити {out}: {e}"))?;
            writeln!(f, "workload,alg,frames,faults,fault_rate,writebacks,ns_per_ref").map_err(|e| e.to_string())?;
            for (name, trace) in workloads() {
                for &frames in &[4usize, 8, 12, 16, 24, 32, 48, 64] {
                    for alg in ALGS {
                        let (st, ns) = simulate(alg, frames, &trace)?;
                        writeln!(f, "{name},{alg},{frames},{},{:.5},{},{:.1}", st.faults, st.fault_rate(), st.writebacks, ns)
                            .map_err(|e| e.to_string())?;
                    }
                }
                eprintln!("готово: {name}");
            }
            Ok(())
        }
        _ => usage(),
    }
}

fn main() {
    if let Err(e) = run() {
        let _ = writeln!(io::stderr(), "Помилка: {e}");
        process::exit(1);
    }
}
