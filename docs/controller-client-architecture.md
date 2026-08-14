# Controller–Client State Synchronization Architecture

## 目的

Controller–Client 間の通信を、Controller が所有する正規 State と、その State に適用される Operation を中心に再構成する。

- Controller と Client は同じ State・Operation 定義を使用する。
- Controller は初期構築後、Operation の適用によってのみ公開 State を変更する。
- Client は State を subscribe し、最初に snapshot、その後に Operation を受信する。
- 状態変更要求は single-shot Command とする。
- TUI は同期済み State を正規データとし、表示専用 cache だけを持つ。
- 旧 protocol との互換性は維持しない。

## 共有定義

State、Operation、Command、subscription message は `atra-protocol` に一元化する。

各 State の field は private とし、read-only getter を公開する。完全な State を作れるのは、snapshot decode と永続データからの materialization に使う明示的 constructor だけとする。

初期構築後の変更は次の API に限定する。

```rust
impl ThreadOperation {
    pub fn apply(self, state: &mut ThreadState) -> ThreadChange;
}
```

Controller と `atra-client` は同じ `apply` 実装を使用する。`ThreadChange` などの ChangeSet は serialization せず、Operation 適用時にローカルで生成する。

## 公開 State

公開 State は一つの巨大な構造にせず、購読単位で独立させる。

### ControllerState

Controller 全体の公開状態を持つ。

- lifecycle: `Running` または `Stopping`
- 順序付き thread 一覧
- provider ごとの login status、models、rate limits
- Runner ごとの `Launching`、`Running`、`Failed`
- 公開される global runtime 情報

Provider credential や provider object、DB handle、lock などの内部情報は含めない。

Provider lifecycle は少なくとも以下を表現する。

- `LoggedOut`
- `LoggingIn`
- `LoginRequired`
- `LoggedIn`
- `Refreshing`
- `Failed`

非同期の provider・Runner failure は、次の同種操作が始まるまで State に残す。

### ThreadState

一つの thread に属する状態を持つ。

- thread metadata
- append-only `ThreadEvent` 列
- optional `ActiveTurn`
- 直近の turn outcome
- checkpoint metadata の順序付き一覧
- managed process summary の順序付き一覧

`ActiveTurn` は turn 全体の複製ではなく、現在未確定の情報だけを持つ。

- `Running`
- `Retrying`
- `AwaitingApproval`
- `Cancelling`
- `Compacting`
- ID 付きの未確定 `ActiveItem` 列
- 最大一件の pending approval

`ActiveItem` は型付き enum とし、少なくとも assistant、reasoning、web search、tool call、Runner tool の未確定状態を表現する。

確定済み item は直ちに `events` へ移す。長い turn でも、`ActiveTurn` に過去の確定 item を蓄積しない。

直近 outcome は `Completed`、`Cancelled`、`Failed` を表現し、次の turn 開始時に消去する。

### CheckpointState

一つの checkpoint とその event 列を持つ。checkpoint は immutable だが、読み取り方式を統一するため通常の subscription として扱う。

### ProcessState

一つの managed process の詳細を持つ。

- process metadata
- lifecycle/status
- bounded output tail
- omitted bytes

すべての長時間 command execution を managed process として扱い、開始 response で `ProcessId` を返す。foreground/background による Client protocol の分岐は設けない。

output 更新は追加 content と先頭から truncate する byte 数を Operation で送る。

## State のロードと永続化

`ControllerState` は Controller 起動時に構築する。

`ThreadState` と `CheckpointState` は初回 subscription または対象 Command の実行時に SQLite から materialize する。一度ロードした State は Controller 終了までメモリに保持する。eviction は行わない。

未ロード resource に対する Command は、先に State を materialize してから実行する。

SQLite に永続化する対象は現行と同じ範囲に限定する。

- thread metadata
- thread events
- checkpoints
- checkpoint events

以下は永続化しない。

- `ActiveTurn`
- 直近 turn outcome
- provider runtime state
- Runner runtime state
- managed process state

Controller 再起動後、thread は既存 events を持つ `Idle` 状態として materialize する。中断を表す人工的な event は追加しない。managed process は Runner 終了時に停止するため復元しない。

## Mutation の順序

永続変更は次の順序で処理する。

1. Command を検証する。
2. private Store API で SQLite transaction を commit する。
3. transaction が確定した ID、sequence、値から View Operation batch を構築する。
4. Controller 内の公開 State に batch を適用する。
5. subscriber queue へ Operation を投入する。
6. Command response を返す。

```text
SQLite commit
    ↓
View Operation batch
    ↓
Controller State
    ↓
Subscriber queues
    ↓
Command response
```

DB commit 後、State 適用前に Controller が crash した場合は、再起動時の materialization により回復する。operation log や rollback operation は追加しない。

永続 mutation と lazy materialization は、Controller 全体で一つの mutation mutex により直列化する。

公開 State 自体には別の単一 lock を使用する。この lock は snapshot clone、subscriber 登録、Operation 適用にだけ使用し、DB、model、Runner I/O 中は保持しない。

一つの変更が複数 View に影響する場合、必要な View Operation を batch として組み立て、State lock 内でまとめて適用する。

例:

```text
Thread rename
├── ControllerOperation::ThreadUpdated
└── ThreadOperation::MetadataUpdated
```

Controller 内では一括適用するが、異なる subscription connection への到着時刻までは同期しない。batch ID や cross-subscription transaction は導入しない。

## Operation の原則

Operation は汎用 patch ではなく、意味を持つ型付き variant とする。

例:

```rust
enum ThreadOperation {
    MetadataUpdated { /* ... */ },
    EventAppended { event: ThreadEvent },
    ActiveTurnStarted { /* ... */ },
    ActiveItemAdded { /* ... */ },
    ActiveTextAppended { id: ActiveItemId, content: String },
    ActiveItemFinalized {
        active_id: ActiveItemId,
        event: ThreadEvent,
    },
    PhaseChanged { /* ... */ },
    ApprovalRequested { /* ... */ },
    ApprovalResolved { /* ... */ },
    TurnFinished { /* ... */ },
    EventsReplaced { events: Vec<ThreadEvent> },
    CheckpointAdded { /* ... */ },
    ProcessUpdated { /* ... */ },
}
```

通常動作では `ThreadState.events` に対して append だけを許可する。同じ sequence の更新や upsert は許可しない。

以下の明示的な履歴操作だけは、完全な event 列を持つ `EventsReplaced` を使用できる。

- compaction
- rewind
- checkpoint restore
- output masking など、履歴置換自体を目的とする操作

生成途中の text は全文置換せず、`ActiveItemId` と追加 chunk を送る。

確定 item には `EventSequence`、未確定 item には `ActiveItemId` を安定キーとして使用する。finalize 時の ChangeSet は `ActiveItemId → EventSequence` の対応を返せるものとする。

Runner の進行状況など、確定 event へ重ねる一時状態は、関連する `EventSequence` または call ID を持つ `ActiveItem` として表現する。確定 event 自体は変更しない。

## Subscription protocol

Unix socket 上では改行区切り JSON を使用する。一行を一 message とし、unknown field、不正な variant、不正な message 順序は厳格に拒否する。

Client が最初に送れる request は二種類だけとする。

```rust
enum ControllerRequest {
    Command(Command),
    Subscribe(Subscribe),
}
```

shutdown は Command の一種とする。

一つの Unix connection は一つの subscription 専用とする。subscription 開始後、Client から追加 message は送信しない。unsubscribe は socket close で表現する。

Controller からの subscription message は次の順序に限定する。

```text
Snapshot → Operation* → Terminal?
```

Terminal は resource deletion、Controller shutdown、subscription error など、理由付きの終了を表す。

subscription 登録は State lock 内で行う。

1. subscriber queue を登録する。
2. 同じ queue の先頭に snapshot を投入する。
3. lock を解放する。
4. 以後の Operation も同じ queue へ投入する。

これにより snapshot と最初の Operation の間に変更の欠落が発生しない。

revision、operation replay、resume token は持たない。Unix stream の順序保証を利用し、再接続時は新しい snapshot から開始する。

subscriber queue は KISS のため unbounded とする。slow-client 用の合成、切断閾値、backpressure protocol は追加しない。

Controller は queue からの送信と peer EOF を同時に監視する。heartbeat と idle timeout は持たない。

## Client API

`atra-client` は型付き subscription object を提供する。

```rust
let mut subscription = client.subscribe_thread(thread_id).await?;

let state: &ThreadState = subscription.state();

let change: ThreadChange = subscription.receive().await?;
let state: &ThreadState = subscription.state();
```

`Subscription` が同期済み State を所有する。利用側へは immutable reference だけを公開する。

`receive()` は次を行う。

1. Operation を decode する。
2. 所有する State へ共有 `apply` を実行する。
3. View 固有の ChangeSet を返す。

TUI へ wire Operation 自体は返さない。decode 不能または適用不能な Operation を受けた場合は subscription error として終了する。互換 fallback や自動再接続は行わない。

inactive thread や閉じた詳細画面の subscription は終了する。同じ resource を再び開く場合は新しい connection と snapshot を使用する。

## Single-shot Command

公開状態の読み取りは原則すべて subscription に統一する。従来の list/query/polling API は残さない。

Command response は成功または文字列 error の一回だけとし、接続を閉じる。

成功 payload は必要最小限の型付き結果とする。

- `Accepted`
- `ThreadCreated { thread_id }`
- `ThreadForked { thread_id }`
- `ProcessStarted { process_id }`

時間のかかる処理では response は完了ではなく開始受理を意味する。

- turn
- compaction
- provider login
- Runner launch
- command execution

進行、完了、失敗は対応 State の Operation と snapshot から観測する。

response 到着前に connection が切れた場合、`atra-client` は自動 retry しない。結果不明 error を返し、利用側は subscription State で反映を確認する。

受理済み処理は Client connection や subscription の寿命と独立して Controller が所有する。

複数 Client から競合する Command が送られた場合、Controller lifecycle と lock により一件だけを受理し、他を error とする。Command queue や後勝ち置換は導入しない。

## TUI cache

TUI は共有 State の shadow copy を持たない。同期済み State を唯一の正規データとし、`atra-tui` 内には表示専用 cache を置く。

```text
ThreadOperation
    ↓ shared apply
ThreadState + ThreadChange
    ↓
TranscriptCache refresh
    ↓
ratatui rendering
```

`TranscriptCache` は以下を保持する。

- stable key ごとの semantic `TranscriptItem`
- Markdown parse、wrap、展開状態を含む rendered result

`ThreadChange` は event sequence、active item ID、call ID などの domain key だけを返す。TUI 固有の `TranscriptKey` は共有 crate に含めない。

ToolCall と ToolResult のような複数 event の表示上の結合は semantic cache が担当する。Runner の live overlay も cache が State を参照して合成する。TUI が Operation の状態遷移を再実装することはない。

通常の Operation では、ChangeSet が示す item だけを再計算する。

- assistant delta: 対象 active item だけ
- tool update: 関連 call ID の item だけ
- finalize: active cache entry を event key へ移す
- metadata update: transcript cache は変更しない

全 cache 再構築を許可するのは以下だけとする。

- initial snapshot
- explicit history replacement

terminal resize では semantic cache を維持し、幅依存の rendered result だけを再計算する。

各 frame で cached item の高さを走査して visible range を求める O(n) 処理は許容する。ratatui frame 自体も従来どおり dirty tick で描画する。避ける対象は transcript 全体の semantic 再構築、Markdown parse、wrap の繰り返しである。

provider delta は batching せず、そのまま Operation として配送する。TUI の描画 tick が複数 Operation を一 frame にまとめる。

## Process synchronization

Runner は managed process の実体を所有し続ける。Controller は process ごとの watcher task を持ち、既存 Runner API を通じて status/output を取得する。

watcher は次を更新する。

- `ThreadState` 内の process summary
- 対応する `ProcessState`

Client polling は行わない。Controller–Runner protocol の push 型への変更は今回の設計範囲外とする。

## Shutdown と resource deletion

Controller shutdown 時は次の順序とする。

1. `ControllerState` に `Stopping` Operation を適用する。
2. 他の subscription に shutdown Terminal を送る。
3. connection を閉じる。
4. Runner lifecycle を終了する。

購読中の Thread または Process が削除された場合は、理由付き `Deleted` Terminal を送って該当 connection を閉じる。deleted 状態を State に残し続けない。

## 非目標

- 旧 `ControllerRequest` / `ControllerResponse` / `TurnStream` との互換性
- version negotiation
- operation replay と永続 operation log
- generic selector や field-path subscription
- generic JSON patch
- Client による optimistic State mutation
- TUI interaction state の同期
- Controller–Runner protocol の全面的再設計
- active turn、approval、process の再起動復元
