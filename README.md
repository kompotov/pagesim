# pagesim

Симулятор алгоритмів заміщення сторінок пам'яті (OPT, FIFO, LRU, CLOCK, NRU, Aging, LFU, Random).
Програма до курсової роботи з дисципліни «Системне програмування» на тему «Алгоритми заміщення сторінок пам'яті».

## Встановлення

Потрібен Rust 1.70 або новіший (https://rust-lang.org, утиліта rustup). Програма працює у Windows, Linux і macOS.

```
git clone https://github.com/kompotov/pagesim pagesim
cd pagesim
cargo build --release
```

Виконуваний файл: `target/release/pagesim` (у Windows `pagesim.exe`).

## Використання

```
pagesim string "7 0 1 2 0 3 0 4 2 3 0 3 2 1 2 0 1 7 0 1" 3 all   # траса з рядка
pagesim run trace.txt 16 lru                                      # траса з файлу
pagesim belady                                                    # аномалія Біледі
pagesim bench results.csv                                         # повний експеримент
```

У трасі номери сторінок відокремлюються пробілами, комами або новими рядками; суфікс `w` позначає запис (наприклад, `5w`).

## Файли

- `src/main.rs` — вихідний код симулятора;
- `results.csv` — результати експерименту, використані в курсовій роботі.
