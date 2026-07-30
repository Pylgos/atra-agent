# Terminal-Bench 2.1によるCodex–Atra比較

CodexとAtraを、同じTerminal-Bench 2.1の20 taskで比較するための
ベンチマークです。モデルは`gpt-5.6-sol`、reasoning effortは`medium`、
attempt数と同時実行数はともに1へ固定しています。

taskは難易度`hard`のものだけを対象とし、分野が偏らないようcategoryを
巡回しながら、expert timeが長いものから20件選んでいます。datasetは
content digestで固定しています。

## ディレクトリ構成

実行ログと、比較に採用する結果を分離しています。

```text
benchmarks/terminal_bench_2_1/
├── jobs/
│   └── <job-id>/                 # Harbor jobの生ログ
│       ├── config.json
│       ├── job.log
│       ├── result.json
│       └── <task>__<trial-id>/
│           ├── trial.log
│           ├── result.json
│           ├── agent/
│           └── verifier/
└── campaigns/
    └── <campaign>/
        ├── campaign.json         # このcampaignに対応するagent
        ├── results/
        │   └── <task> -> ../../../jobs/<job-id>/<trial>
        └── results.csv
```

`jobs/`は実際に実行されたHarbor jobを追記していく場所です。
`campaigns/`は比較に採用するtrialの集合で、`results/`には`jobs/`内の
trialを指す相対symlinkだけを置きます。

1 campaignは1 agentだけに対応します。同じcampaignを別のagentで
使うことはできません。campaign間の結果探索や共有、Git revisionによる
自動選択は行いません。

## 前提条件

- DockerとCompose plugin
- Python 3.12以降と`uv`
- `codex login`済みのCodex 0.146.0
- Nixでビルドでき、`atra codex login`済みのAtra

Atraを初めて使う場合は、リポジトリrootで認証します。

```bash
nix build .#atra --out-link result-atra
result-atra/bin/atra codex login
```

AtraのControllerと認証情報はhostに残ります。task containerへ配置するのは
static Runnerだけで、taskのcommandとpatchはHarborが検証するcontainer内で
実行されます。Terminal-Benchのtaskはinternet accessを許可しているため、
CodexとAtraのWebSearchもどちらも有効にしています。

## 実行

campaign名とagentは必ず明示します。まず1 taskのpilotを実行します。

```bash
./benchmarks/terminal_bench_2_1/run.py pilot \
  --campaign codex-baseline --agent codex

./benchmarks/terminal_bench_2_1/run.py pilot \
  --campaign atra-current --agent atra
```

問題がなければ、同じcampaignで20 taskを実行します。pilotで採用済みの
taskはskipされるため、それぞれ最大19 trialです。

```bash
./benchmarks/terminal_bench_2_1/run.py full \
  --campaign codex-baseline --agent codex

./benchmarks/terminal_bench_2_1/run.py full \
  --campaign atra-current --agent atra
```

実行前に、採用済み、保留中のerror、今回実行するtrial数を表示します。
providerを呼ぶ前に確認を求めるため、想定外のtrial数なら中断できます。
`--dry-run`ではDocker、Nix、providerを呼ばずにplanだけ確認します。

## 再実行

正常終了したtrialは、campaignの`results/`にsymlinkがある限りskipします。
Atraのコード、モデル、effortなどを変更しても自動では再実行しません。
新しい結果へ置き換える場合はagentとcampaignを明示して実行します。

```bash
./benchmarks/terminal_bench_2_1/run.py full \
  --campaign atra-current --agent atra --rerun-completed
```

errorになったtrialも`results/`へ採用し、通常実行では保留します。errorだけを
再実行する場合は次のようにします。

```bash
./benchmarks/terminal_bench_2_1/run.py full \
  --campaign atra-current --agent atra --retry-errors
```

再実行前のjobは`jobs/`に残り、campaignのsymlinkだけが新しいtrialを指す
ように更新されます。そのため、過去に何を実行したかは失われません。

## レポート

各実行後、そのcampaignで現在採用しているtrialだけを集計し、
`results.csv`へ保存します。2 agentを比較する場合は、2 campaignの
`results/`をレポーターへ渡します。標準出力にはagent単位の集計と、
taskごとのstatus、reward、使用量を整列済みMarkdownで表示します。

```bash
python3 benchmarks/terminal_bench_2_1/report.py \
  benchmarks/terminal_bench_2_1/campaigns/codex-baseline/results \
  benchmarks/terminal_bench_2_1/campaigns/atra-current/results
```

全jobの消費量やerrorも含めて調査したい場合は、`jobs/`を直接渡せます。

```bash
python3 benchmarks/terminal_bench_2_1/report.py \
  benchmarks/terminal_bench_2_1/jobs
```

`input_tokens`にはcached inputが含まれます。`uncached_input_tokens`は
`input_tokens - cached_input_tokens`です。Atraのrequest数はevent metadata、
Codexのrequest数はsession内の`token_count` eventから集計します。

quotaはCodex weekly windowの`used_percent`について、最初と最後に観測した
値を表示します。最初のrequestによる増加は観測できず、値の分解能は
1 percentage pointです。windowの切り替わりやresetを検出した場合は
`reset/unknown`と表示します。

詳細ログは各trialの以下の場所にあります。

- 共通: `trial.log`、`result.json`、`verifier/test-stdout.txt`
- Atra: `agent/atra-controller.log`、`agent/atra-events.jsonl`、
  `agent/atra-output.txt`
- Codex: `agent/codex.txt`、`agent/trajectory.json`、
  `agent/sessions/**/*.jsonl`

[Terminal-Bench 2.1](https://github.com/harbor-framework/terminal-bench-2-1)
および
[Harbor dataset](https://hub.harborframework.com/datasets/terminal-bench/terminal-bench-2-1/6)
を使用しています。
