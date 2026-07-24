# Codex向けタスク: Atra Agentを実装する

このリポジトリで **Atra Agent** を実装してください。

この文書には、現時点で合意済みのアーキテクチャと実装計画だけを記載しています。現在の仕様の正本として扱ってください。この文書で必要とされていない製品機能、汎用フレームワーク、互換レイヤー、データベースフィールド、プロトコルメッセージ、抽象化を追加しないでください。

Atraは個人開発者向けのツールです。マルチテナント基盤や企業向けオーケストレーションシステムではありません。記載された動作を完全に実現できる、最小の設計を優先してください。

実装はstageごとに進めます。
各stageを開始する前に、設計判断が必要な点をまとめ、ユーザーと相談してください。仕様に曖昧さがある場合や追加の設計判断が必要な場合も、推測で進めず、その時点で積極的に判断を仰いでください。
stageが完了したら作業を止め、ユーザーへ確認を依頼してください。

## 1. 絶対的な実装原則: 過剰設計を避ける

型、テーブル、フィールド、trait、プロトコルメソッド、サービス、状態機械、互換処理、設定レイヤーを追加する前に、それを今必要としている機能を特定してください。

将来役立つかもしれないという理由だけで追加しないでください。

特に、以下は禁止します。

- 古いAtraのプロトコル、schema、command、開発版との互換性を維持しない。
- deprecated aliasや、明示されたCodex `apply_patch`互換性以外の寛容なparserを追加しない。
- 汎用remote execution frameworkを作らない。
- 汎用filesystem RPC APIを作らない。
- まだ存在しない機能のために詳細な永続化schemaを作らない。
- 後続タスクで明示されない限り、branch、compaction、subagent orchestration、remote authentication、Web API、plugin system、migration frameworkを追加しない。
- 小さな実装詳細を公開architectureへ昇格させない。
- 現時点で実装が一つしかなく、直ちにtest boundaryやdependency boundaryとして必要でもない抽象化を作らない。
- 二つの設計が要件を満たす場合、概念と永続状態が少ない方を選ぶ。

目的は、雑に機能を減らすことではありません。必要な機能を、最小限の仕組みで実現することです。

## 2. 命名

プロジェクト名:

```text
Atra Agent
```

CLI:

```sh
atra
```

公開component名:

```text
Atra Client
Atra Controller
Atra Runner
```

公開component名として`core`や`proxy`を使用しないでください。

## 3. アーキテクチャ

```text
Atra Client
     │
     │ local commandとevent
     ▼
Atra Controller
     │
     │ child processのstdio上のRunner protocol
     ▼
Atra Runner
     │
     ├─ Bash commandの実行
     ├─ 実行中commandの管理
     └─ Atra patchの適用
```

### Client

最初のClientは全画面terminal UIです。

### Controller

Controllerは長時間動作するlocal processです。以下を所有します。

- 会話履歴
- Atra独自のagent loop
- model provider呼び出し
- tool routing
- Runnerの登録とlifecycle
- Runnerごとのapproval policy
- Clientとのapproval interaction
- 最小限の永続化

Controllerはworkspaceごとに一つ持ちます。現時点ではworkspace root探索を行わず、`atra`を起動したcanonicalなcwdをworkspaceとします。

### Runner

Runnerはhost、sandbox、container、remote command環境内で動く小さな実行processです。

会話、model call、approval policyを所有しません。

会話を一つのRunnerへ固定しません。各tool callが実行先Runnerを選択します。

## 4. 言語と最初のClient

Controller、Runner、CLI、最初のClientにはRustを使用します。

最初のClientはRatatuiとCrosstermを使った全画面TUIです。

TUIは、他のClientと同じ公開local protocolを通してControllerと通信します。TUIからControllerの内部実装を直接呼び出さないでください。

このタスクではDesktop ClientとWeb Clientを実装しません。

## 5. Controllerのlifecycleとlocal IPC

canonicalなController command:

```sh
atra controller start
atra controller stop
atra controller run
atra controller status
```

現時点では`run`と`status`が実装済みです。`start`と`stop`はcanonical commandとして後続stageで実装します。

Client–Controller間のlocal通信にはUnix Domain Socketを使います。

runtime directoryとsocketのfilesystem permissionをlocal security boundaryとして使います。local Runner起動のためのtoken systemは追加しないでください。

socket endpointはcanonical cwdのSHA-256 digest先頭16桁を使い、以下に配置します。

```text
$XDG_RUNTIME_DIR/atra/<workspace-digest>/controller.sock
```

`XDG_RUNTIME_DIR`がない場合は`/tmp/atra-<uid>/<workspace-digest>/controller.sock`を使います。directoryとsocketには現在のユーザーだけがアクセスできるpermissionを設定します。

XDG directoryの解決には小さな`xdg` crateを使い、環境変数fallbackを独自に増やさないでください。

Controller stateは以下に配置します。

```text
$XDG_STATE_HOME/atra/<workspace-digest>/controller.sqlite3
```

古いAtra versionとのprotocol compatibilityは不要です。ControllerとRunnerは常に同時配布するため、protocol version field、version negotiation、adapterを実装しないでください。

このタスクではpublic network protocolを設計しません。

## 6. 名前付きRunnerと`atra runner launch`

一つのsetup scriptから、動的に0個以上のRunnerを作成できます。

例:

- raw host Runner
- `bwrap`が存在するときのBubblewrap Runner
- 適切なcontainerが存在するときのcontainer Runner

各Runnerは安定したユーザー向け名称を持ちます。

canonicalな形:

```sh
atra runner launch \
  --name host-raw \
  --approval ask \
  -- "$ATRA_RUNNER_BINARY" --stdio
```

`atra runner launch`は冪等なreconcile操作です。

- 名前付きRunnerのController所有設定を更新する。
- 同名のlive Runnerが既に存在する場合、新しいRunnerを起動しない。
- 存在しない場合、指定されたcommandを起動する。
- commandのstdin/stdoutを一つのRunnerとしてControllerへ接続する。
- Runnerのlifecycle管理はControllerが行う。

これにより、新しいRunnerを追加した後でも、既存Runnerをすべて再起動せずsetup scriptを再実行できます。

一つのsetup scriptから`atra runner launch`を任意回数呼び出せます。起動数や宣言的launcher fileは要求しません。

setup scriptを短く保つため、launch commandは共通値を標準で環境変数から参照します。少なくともController endpointとRunner binary pathにdefaultを持たせてください。

approval policyはlaunch commandで指定し、Controllerが保持・判定します。Runner自身がapproval policyを選択したり、権限を広げたりしてはいけません。

最初のapproval policyは以下の二つです。

```text
ask
allow
```

Runner設定は起動時またはControllerからのmessageで設定し、Runnerには永続化しません。Controllerも現時点では設定fileを持たず、launchのたびにmemory上の設定を更新します。設定変更によるlive Runnerの再起動は不要です。

Docker、Podman、Bubblewrap、SSH固有のlauncher logicをControllerへ追加しないでください。これらはsetup scriptが渡す通常のcommandです。

## 7. Runner transport

Runnerは標準入力と標準出力上でAtra Runner protocolを話します。

```text
stdin   Controller → Runner
stdout  Runner → Controller
stderr  log
```

Runnerはstdoutへlogを書いてはいけません。

Controllerが任意commandを起動し、そのstdio connectionを直接所有します。永続的なrelay CLI processは不要です。

Runner protocolはstrictかつ小さく保ってください。現在必要なcommand実行とpatch適用だけを支えれば十分です。

同じRunnerで複数threadのtool callを同時に処理できるよう、Controller–Runner間のrequestには内部的な`request_id`を持たせ、responseを多重化します。これはユーザー向けprocess handleやprotocol versionではありません。

実装はTokioを使用します。

## 8. Containerへのdeploy

container imageにAtra Runnerが含まれていることを必須にしません。

Runner binaryのhost bind mountも必須にしません。

想定するcontainer flow:

1. 現在のarchitectureに対応するplatform bundleからRunner binaryを選択する。
2. 起動済みcontainerの`sh`とstdinを使ってbinaryをstream転送する。
3. content-addressedな実行可能pathへ配置する。
4. 同じbinaryが既に存在する場合はuploadを省略する。
5. `docker exec -i`、`podman exec -i`、または同等commandで起動する。
6. そのcommandを`atra runner launch`へ渡す。

`sh`がないcontainerは初期対象外です。

upload用bootstrapではcontainerに既存の`sh`を使用できます。Runnerがagent commandを実行するときはBashを使用します。

## 9. Command実行モデル

model-facing command toolは以下です。

```text
exec_command
wait_process
write_process
stop_process
apply_patch
```

process操作をaction dispatch型の一つのtoolへ統合しないでください。それぞれのschemaは大きく異なります。

### `exec_command`

必要な概念:

```text
runner
command
optional cwd
background
timeout
timeout時の動作
```

commandは以下で実行します。

```sh
bash -lc '<command>'
```

Bashが存在しない場合、RunnerをReadyとする前にAtraがbundleしたBashをdeployできます。

`exec_command`は二つのmodeを持ちます。

#### Foreground mode

指定したtimeoutまでcommandの完了を待ちます。

commandが完了した場合、outputとexit statusを返します。

timeout時にも実行中なら、指定された動作に従います。

```text
return_running
terminate
```

`return_running`はprocessをRunner管理下に残し、それまでのoutputとprocess handleを返します。

`terminate`はprocess groupを停止し、timeout resultを返します。

#### Background mode

commandを開始し、速やかにprocess handleを返します。

`process_handle`はRunner内だけで意味を持つopaqueな値です。Linux PIDと誤解される`process ID`という名称をprotocolやUIで使わないでください。

modelがすべての長時間commandを正確に予測することを要求しません。foreground commandが予想外にtimeoutした場合も、`return_running`によってmanaged processへ移行できます。

### `wait_process`

追加outputまたはprocess完了を、指定した有限時間だけ待ちます。

まだ実行中なら、新しいoutputとrunning statusを返します。

終了していれば、残りのoutputとexit statusを返します。

これによりagentは別作業を行い、後からprocessを確認できます。

### `write_process`

managed processのstdinへtextまたはbytesを書き込みます。

stdin対応のためだけにPTY protocolを追加しないでください。

### `stop_process`

process group全体を停止します。通常終了を試し、必要なら強制終了します。

具体的な要件が現れるまで、任意Unix signalを公開しないでください。

### Managed processのlifetime

managed processはRunnerが所有します。

- commandごとにprocess groupを作成する。
- stdoutとstderrをprocess開始時から一つのstreamへmergeし、観測された順序を保つ。通常は両者を区別しない。
- stdin書き込みを許可する。
- Runner終了時にmanaged processを停止する。
- RunnerまたはControllerの再起動をまたいでprocess stateを永続化しない。

Controllerは操作とeventをrouteしますが、自身ではcommandを実行しません。

直接利用にも有用なため、model-facing toolとは別に`atra runner exec`、`wait`、`write`、`stop`をCLIとして公開します。

## 10. tmuxとPTY

`tmux`をbundleするか、Runner環境で利用可能にします。

tmuxは通常の`exec_command`から操作します。tmux専用のAtra APIを追加しないでください。

通常commandにはRunner-managed processを使います。予想外に長時間実行されたcommandもこれに含みます。

agentが意図的に永続background serviceやterminal-oriented interactive sessionを必要とする場合はtmuxを使います。

初期版ではController–Runner間のPTY protocolを実装しません。Bash、managed stdin、tmuxでは十分に扱えない具体的なworkflowが見つかった場合にだけ追加してください。

## 11. Runnerの最小責務

Runnerに必要なのは以下だけです。

- handshakeとreadiness
- command start
- command outputとexitの通知
- process wait
- process stdin write
- process stop
- Atra patchの適用
- 必要な環境を提供するためのbundle toolの内部install

以下のような汎用RPCは公開しないでください。

```text
readFile
writeFile
listDirectory
walk
copy
remove
stat
```

agentは`rg`、`sed`、`cat`、`ls`などのBash commandでファイルを読み、検索し、一覧します。

ファイル編集には`apply_patch`を使います。

tool pack展開では内部的にfilesystem操作を使えますが、公開汎用filesystem serviceにはしません。

## 12. Atra patch

Atra patchは二つのtarget指定形式を持ちます。どちらも正式なfirst-class機能であり、互換modeではありません。

### Content-based hunk

line numberが不明な場合、または小さな編集をcontextで記述する方が安い場合に、周辺source contentを使います。

参照用Codex実装の`apply_patch`入力構文をすべて受理してください。少なくともAdd、Delete、Update、Move、`@@` context、`*** End of File`、複数file、複数chunk、heredoc wrapperを含みます。context探索はCodexと同様に、完全一致、末尾空白無視、前後空白無視、一般的なUnicode punctuationの正規化の順で照合します。

### Numbered range hunk

freshなline numberを使い、境界source lineだけを再掲して長い連続範囲を置換します。

canonicalなrange形式:

```diff
*** Begin Patch
*** Update File: src/main.rs
@ start 123
-fn old_function() {
@ end 156
-}
+fn new_function() {
+    new_call();
+}
*** End Patch
```

startとendのline numberは、同じ適用前file snapshotを基準とします。

境界source lineは完全一致が必要です。

rangeはinclusiveです。

省略された内部範囲を`+`bodyで置換します。

一行rangeでは`@ end`を省略できます。

同じfile snapshotを対象とするhunkのoverlapは拒否してください。

file operationはpatchに書かれた順序で適用します。途中で失敗した場合、それ以前に成功した変更は保持し、atomic rollbackは行いません。

Codex互換の空白差許容以外に、旧Atra構文aliasや根拠のない自動修復を追加しないでください。

patch適用はRunnerの責務です。実装は単独利用者しかいない間は`atra-runner`内のprivate moduleに保ち、独立crateへ分割しないでください。

model-facing `apply_patch`にもRunnerごとのapproval policyを適用します。直接利用向けに`atra runner apply-patch --name ... [--cwd ...]`を提供し、patch本文はstdinから読みます。

## 13. Platform bundle

`rg`、`fd`、`jq`、`tmux`、BashをRunner executableへ直接埋め込まないでください。

Controllerのdistributionは、manifest、Runner、toolごとの独立圧縮blobを含むplatform別ZIP bundleを持ちます。

install済みbundleは`$XDG_DATA_HOME/atra/platforms/<platform>/`以下へcontent-addressedに配置し、現在選択中のbundleを小さな`current` fileで指定します。

ControllerはRunner環境に不足しているtoolだけを送ります。

Runnerはcontent digestを検証し、content-addressedな場所へtoolを展開し、そのdirectoryをcommand PATHの先頭へ追加します。

初期tool:

```text
bash
rg
fd
jq
tmux
```

汎用package managerを作らないでください。Atra用の小さな固定tool bundleです。

## 14. Model provider

Atraは独自のconversation model、agent loop、tool routing、approval flowを所有します。

最初のreal model providerはユーザーのCodex subscriptionを使用します。

Codex app-serverにAtraのagent loopを所有させないでください。

Codex subscription integrationは狭いmodel-provider boundaryの後ろへ隔離し、testでは決定的なfake providerへ差し替えられるようにします。

subscription要件をAPI key課金へ黙って置き換えないでください。

実装には再利用可能なCodexのauthenticationまたはmodel client codeが必要になる可能性があります。その依存だけを隔離し、Codexのconversation、tool、approval architectureをAtraへ取り込まないでください。

## 15. 最小限の永続化

永続化は意図的に小さく保ってください。

会話を再度開き、model-visibleおよびuser-visible historyを再構築するために現在必要な情報だけを保存します。

最初は以下だけです。

```text
threads
threadに属する順序付きevent
```

storageにはSQLiteを使います。一つのControllerが順序付きeventへ追記しつつ複数Clientから読み書きするため、transaction、同時access、query、durabilityを自前で再実装せずに済むことを優先します。

event streamには、現在使用するevent kindだけが必要です。

- user message
- assistant message
- tool call
- tool result
- approval requestとresponse

Turn、branch、summary、blob、subagent、projection、context snapshot、将来のschedule用に、詳細tableや必須fieldを作らないでください。

live process stateは永続化しません。

pending approvalとRunner設定もController再起動をまたいで復元しません。

現在実装中の機能が、それなしでは正しく動かない場合にだけfieldやtableを追加してください。

初期開発中のdatabaseは実装詳細です。破棄される開発schemaのためにmigration frameworkを作らないでください。

## 16. TUIとコピー

最初のClientは全画面TUIです。

terminal scrollbackを会話storeとして利用しません。

コピー対象はRatatuiのrendered cell bufferではなく、論理的なtranscript textから生成します。

visual selectionをsource textへ変換するために必要な最小限のlayout mappingを保持し、以下を満たしてください。

- soft wrapによる改行を挿入しない。
- borderとpaddingを含めない。
- 元の改行を保持する。
- visual decorationをコピーしない。

初期clipboard writeにはOSC 52を使います。

modelとcommandのoutputはrender前にsanitizeしてください。信頼できないescape sequenceをterminalへ直接流してはいけません。

clipboard readは実装しません。

## 17. Test

通常の自動testではlive Codex subscriptionを使用しません。

### Unit test

pure logicには通常のRust unit testを使用します。

細かいtestを大量に先回りして追加しないでください。必要最小限のtestをstageごとに徐々に追加します。実際の利用形態を通る少数のprocess-level integration testを、重複する細粒度testより優先してください。

### Integration test

Rust integration testを`cargo test`から実行します。

以下を使用します。

- real Controller libraryをwrapするreal Controller processまたは薄いfixture binary
- 決定的なfake model provider
- real Runner libraryをwrapするreal Runner processまたは薄いfixture binary
- real Unix socket
- real child-process stdio
- temporary directoryとtemporary database

fixture binaryが必要な場合、integration-test package内へ配置し、Cargoにbuildさせて`CARGO_BIN_EXE_*`からpathを取得します。

test内部から`cargo run`を実行したり、Cargoを再帰起動したりしないでください。

最初のcommand execution integration testでは以下を確認します。

1. 短いcommandが完了し、outputを返す。
2. foreground commandがtimeoutし、running process handleを返せる。
3. background commandがrunning process handleを返す。
4. 最初のprocessがactiveな間に別commandを実行できる。
5. `wait_process`が後から完了を観測する。
6. `write_process`がstdinへ到達する。
7. `stop_process`がprocess groupを停止する。
8. Runner終了後にmanaged processが残らない。
9. Controller再起動後も会話eventが残る。
10. 同じRunnerへの長いwaitが別requestをblockしない。
11. stdoutとstderrのmerged outputが観測順を保つ。
12. Codex互換patch、numbered range、途中失敗時の部分適用が動作する。
13. `apply_patch`を含むmodel tool callがRunner policyに従ってapprovalされる。

Controller processのstderrはtest workspace内のlog fileへ書き、testがpanicした場合だけ出力してください。pipe bufferによって子processを停止させないでください。

Unix socket bindを禁止するsandbox内ではprocess-level integration testが失敗します。その場合、timeoutを延ばして隠さず、許可されたsandbox外実行で検証してください。controller socketの起動待ちは1秒のままにします。

live Codex-subscription testは、手動またはagentが明示的に実行します。通常CIには含めません。

container、tmux、terminal固有動作は、default test frameworkを拡大せず、必要に応じてfocused manual smoke testで確認できます。

## 18. 初期実装順序

小さなvertical sliceを実装してください。sliceが動く前に、将来用crateや機能をすべてscaffoldしないでください。

推奨順序:

1. [完了] sliceに必要な最小限のRust workspaceと共通protocol typeを作る。
2. [完了] `atra controller run`、local Unix socket、status処理を実装する。
3. [完了] 最小限のthreadとordered event persistenceを実装する。
4. [完了] `atra-runner --stdio`を実装する。
5. [完了] `atra runner launch --name ... -- COMMAND...`を実装する。
6. [完了] 短いforeground command向けの`exec_command`を実装する。
7. [完了] managed running processと三つのprocess toolを追加する。
8. [完了・9と同時実装] 決定的なfake model providerとprocess-level integration testを追加する。
9. [完了・8と同時実装] fake providerを使う最小限のAtra agent loopを接続する。
10. [完了] Runnerごとのpolicyに必要な範囲だけapproval routingを追加する。denyはoptionalなreasonを受け取れる。
11. [完了] Atra patchを実装する。
12. [完了] 最初の全画面TUIとOSC 52 copyを実装する。
13. [完了] platform bundle deployとcontainerへのRunner uploadを実装する。
14. [完了] real Codex-subscription providerを統合し、手動確認する。

architectureを完成済みに見せるためだけに後続stageを先回りして作らないでください。

各stageの終了時には、追加の抽象化より動作とtestを優先してください。

初期実装stage 1–14は完了しています。

## 19. 実装上の共通方針

application boundaryのerrorには`anyhow`とcontextを使い、デバッグ可能なerror chainを保ってください。storageなど、呼び出し側がerror kindを実際に判定する狭いboundaryだけtyped errorを使用します。用途のない大きなerror enumを先に作らないでください。

loggingには`tracing`を使い、stdoutをprotocol専用に保つためstderrへ出力します。`RUST_LOG`を尊重し、default levelは`info`です。command本文は`debug`、stdinやpatch本文は`trace`で記録できます。Runner stderrはControllerが読み、Runner名を付けてController logへ流します。command、stdin、patchはtranscriptにも残るため、これらをlog対象から一律除外する必要はありません。

workspaceには現在、役割ごとに以下のcrateがあります。

```text
atra
atra-controller
atra-protocol
atra-runner
```

機能を独立crateにするのは複数の実利用者や明確なdependency boundaryが生じた場合だけにしてください。

## 20. 未指定事項の扱い

この文書に詳細がない場合:

1. 現在のstageを支える最小実装を選ぶ。
2. public APIへ昇格させずprivateに保つ。
3. 導出可能またはmemoryで管理可能なら永続化しない。
4. ユーザーが今制御する必要がなければ設定を追加しない。
5. 設計を正当化するために将来機能を捏造しない。
6. 重要なlocal assumptionだけを短いarchitecture noteへ残す。

大きな新subsystemが必要に見える場合、Bash、tmux、既存Controller、既存Runner process機構ですでに実現できないか先に確認してください。

Atraのcodebaseは一人が読んで理解できる状態を維持してください。技術的に柔軟でも、「FizzBuzz Enterprise Edition」のような解決策は設計失敗です。
