# stillo のビルド定義。
#
# flake の packages と overlay の両方から同じ定義を使うために切り出している。
# overlay 経由で外部の nixpkgs と組み合わせるときは callPackage がこのファイルを読むので、
# 呼び出し側の nixpkgs の rustPlatform が使われる。
{
  lib,
  rustPlatform,
  version ? (lib.importTOML ../Cargo.toml).workspace.package.version,
}:

let
  # ソースはビルドに必要なものだけ明示的に渡す。
  #
  # flake を git 経由で参照すると .gitignore で target/ や node_modules が除かれるが、
  # path: 経由（ローカルの path input や builtins.getFlake "/abs/path"）では
  # フィルタが効かず、target/ を数GB単位で store にコピーしてしまう。その target の中に
  # 前回のビルド成果物が入っているとチェックが巻き込まれて壊れる。
  # ここで絞っておけば参照経路によらず同じ derivation になる。
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../config
      ../README.md
    ];
  };
in
rustPlatform.buildRustPackage {
  pname = "stillo";
  inherit version src;

  cargoLock.lockFile = ../Cargo.lock;

  # ワークスペースには core/fetcher/renderer/llm/mcp/cli があるが、配布するのは CLI バイナリだけ。
  # 他クレートは path 依存として必要分だけ一緒にビルドされる。
  cargoBuildFlags = [ "--package" "stillo" ];

  # チェックは sandbox で完結する。core の単体テストは include_str! で取り込んだ
  # フィクスチャだけを使い、fetcher の Playwright 統合テストは node_modules と node が
  # 無い環境では自分をスキップする（どちらもソースに含まれない）。
  doCheck = true;
  cargoTestFlags = [ "--workspace" ];

  meta = {
    description = "AI-native terminal browser that extracts page content as Markdown";
    homepage = "https://github.com/stillo-browser/stillo";
    license = with lib.licenses; [ mit asl20 ];
    mainProgram = "stillo";
    platforms = lib.platforms.unix;
  };
}
