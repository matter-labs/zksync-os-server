Basic loadtest to measure the sequencer throughput. See `main.rs` for CLI arguments.

Built by LLM.

Is not included in workspace as we don't want to use some packages used by this tool (e.g. Ethers) 

to run:
```
cargo run --release -- --rpc-url 'http://127.0.0.1:3050' \
--rich-privkey 0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110 \
--duration 240m \
--max-in-flight 50 \
--wallets 100 \
--dest random
```

output example:
```

⏱    8s | sent  157886 | in-fl  4894 | incl  152992 | TPS10 15299.2 | TPSavg 19123.5 | sub p50 10s   1 ms / tot   1 | inc p50 10s  0.20 s / tot  0.20 s
⏱    9s | sent  174588 | in-fl  3872 | incl  170716 | TPS10 17071.6 | TPSavg 18967.2 | sub p50 10s   1 ms / tot   1 | inc p50 10s  0.20 s / tot  0.20 s
⏱   10s | sent  191387 | in-fl  4307 | incl  187080 | TPS10 18708.0 | TPSavg 18707.4 | sub p50 10s   1 ms / tot   1 | inc p50 10s  0.20 s / tot  0.20 s
⏱   11s | sent  209980 | in-fl  4836 | incl  205144 | TPS10 18433.5 | TPSavg 18648.8 | sub p50 10s   0 ms / tot   1 | inc p50 10s  0.20 s / tot  0.20 s
⏱   12s | sent  227788 | in-fl  4429 | incl  223359 | TPS10 18182.2 | TPSavg 18612.9 | sub p50 10s   0 ms / tot   1 | inc p50 10s  0.20 s / tot  0.20 s
⏱   13s | sent  245025 | in-fl  4110 | incl  240915 | TPS10 17978.7 | TPSavg 18531.6 | sub p50 10s   0 ms / tot   0 | inc p50 10s  0.20 s / tot  0.20 s
⏱   14s | sent  262573 | in-fl  2970 | incl  259603 | TPS10 19847.5 | TPSavg 18542.8 | sub p50 10s   0 ms / tot   0 | inc p50 10s  0.20 s / tot  0.20 s
⏱   15s | sent  280715 | in-fl  3868 | incl  276847 | TPS10 17563.2 | TPSavg 18456.0 | sub p50 10s   0 ms / tot   0 | inc p50 10s  0.20 s / tot  0.20 s

```