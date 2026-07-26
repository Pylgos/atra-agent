# Atra Agent

## Design

- Controller は会話、agent loop、model provider、approval、永続化、Runner の lifecycle を所有する。Runner は command、managed process、patch の実行だけを担う。
- 会話は Runner に固定せず、tool call ごとに実行先を選ぶ。Client は公開 local protocol 経由で Controller と通信する。
- approval policy は Controller が判定する。managed process は Runner が所有し、Runner 終了時に停止する。
- Atra 自身が agent loop と tool routing を所有する。

## Rules

- 個人開発者向けのツール。過剰設計は避ける。
- 現在の要件に必要ない型、field、trait、protocol、設定、永続化、互換処理を追加しない。
- 実装詳細は private に保つ。
- 旧 Atra との互換性を維持せず、version negotiation、adapter、alias、寛容 parser を追加しない。
- Controller 自身で command を実行しない。ツールの数は必要最小限に絞り、主に shell command と `apply_patch` を使う。
- 課金を避けるため、自動 test では実際の provider を使わない。integration test から Cargo を再帰起動しない。
- 同じ要件を満たすなら、概念と永続状態が少ない設計を選ぶ。
- 場当たり的な機能追加は避け、リファクタリングを積極的に行う。
- prompt caching を維持するため、通常動作で会話の event 列を更新、削除、再配置せず append-only にする。compaction、rewind、checkpoint restore など履歴の置換自体を目的とする明示的な操作だけを例外とする。

## Workflow

- 機能の追加
  - 既存コードを読んでルールに沿った方針を考える。
  - 何を実装するか、どのように実装するかを説明する。
  - ユーザとの対話を通して実装方針を洗練させる。
  - ユーザの明示的な同意を得てから実装を開始する。
  - 実装が完了したらユーザへ内容を説明し、確認を依頼する。方針から外れた点、拡張した点を必ずユーザに報告する。
