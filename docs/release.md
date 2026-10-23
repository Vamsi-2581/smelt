# 发版

推 `v*` tag 触发 `.github/workflows/release.yml`，产物都传到同一个 GitHub Release：

| 产物 | job | 说明 |
|---|---|---|
| `Smelt.dmg` | `build`（macos-14） | `make dist-build`，用 secrets 里的自签名证书签名（`CODESIGN_P12_*`） |
| `smeltd-<版本>-linux-{x86_64,aarch64}.tar.gz`、`install.sh` | `linux` → `linux-release` | 两种架构各自原生构建，见 `docs/archive/remote-runtime.html` 的 D7 |
| `Smelt-<版本>-android-arm64.apk` | `android` → `android-release` | 正式签名（`SMELT_ANDROID_*` secrets），没配签名直接失败 |
| `SHA256SUMS` | `linux-release` → `checksums` | 先只含 Linux 包（一键安装和运行时自动升级马上能用），全部产物到齐后重写成覆盖所有产物 |

## 发版步骤

tag 必须**正好**是 `v<Cargo.toml 的工作区版本>`，预发布也一样：桌面请远程运行时升级时，运行时按桌面的版本号
去同名 tag 的 Release 下载 `smeltd-<版本>-linux-<架构>.tar.gz`，版本号里少了 `-rc.N` 就找不到包。版本号不要手改，
统一用 `scripts/release.sh`：

```sh
scripts/release.sh --rc        # 预发布：2026.1023.0-rc.1，已有进行中的 rc 就 rc.2、rc.3…
scripts/release.sh             # 正式版：有进行中的 rc 就去掉 -rc.N 发它，否则取今天的新号
scripts/release.sh --dry-run   # 只看会取到哪个号
```

脚本在本地 `main` 上改 `Cargo.toml`、跑 `cargo update --workspace` 更新锁文件（Linux 打包用 `--locked`），只提交这两个
文件并打 tag，最后打印推送命令（`git push --atomic <远端> main v<版本>`），确认后自己推。只提交这两个文件，工作区里
别的未提交改动不受影响。直接推 `main` 会绕过 PR 和 `ci-ok` 检查，需要管理员权限；发版流水线自己会完整构建一遍。

tag 带 `-` 后缀就发成预发布：不会成为 Latest，Linux 一键安装默认也不会装它（要装得设 `SMELT_VERSION`）。

## 发布说明

由 `build` job 的「Release notes」步骤生成，不用 GitHub 默认的起点（它往回找上一个正式版，而 v0.6.6–v0.6.14
不在现在 main 的历史上，会一路退回 v0.6.5，把早就发过的 PR 再列一遍）。起点按版本号往前找第一个是本次祖先的 tag：
预发布从上一个 tag（含 rc）算起，正式版从上一个正式版算起，所以正式版的说明涵盖它之前所有 rc 的改动。

## 版本号

按日期取号，格式 `YYYY.MDD.N`：

- `YYYY` 四位年份；`MDD` 是月份接两位日期（10 月 23 日是 `1023`，1 月 3 日是 `103`）；`N` 是当天第几个版本，从 `0` 起。
  例：`2026.1023.0`、同一天再发 `2026.1023.1`。
- `MDD` 不补成四位：Cargo 要求合法的 semver，各段不能有前导零（`0103` 会被拒）。月份在前、日期固定两位，所以
  不同日子不会撞号，数值大小也和日期先后一致。
- 取的是开始发这个版本那天（打第一个 rc 那天）的日期；正式版沿用 rc 的号，只去掉 `-rc.N`。
- 版本号只表示先后，不表示兼容：桌面和运行时兼不兼容看控制协议版本，不看它。v0.9.1 之后改用这套，
  数值上 `2026.x` 比 `0.9.x` 大，运行时「只升不降」和自动更新都照常。

- 桌面与运行时：`Cargo.toml` 的 `version`。
- Android：versionName 是 tag 去掉 `v`（含 `-rc.N`，与工作区版本相同）；versionCode 是 release 流水线的 `run_number`，逐次递增，
  新包总能覆盖安装旧包。`mobile/pubspec.yaml` 里的版本只给本地开发构建用。

## Android 签名

APK 必须一直用同一把 keystore 签名，否则已装的用户只能卸载重装、数据丢失。keystore 生成于 2026-10-19，
证书 SHA256 指纹 `F3:68:94:C3:7F:AC:49:70:9A:FE:3D:93:41:09:96:36:52:F0:84:DC:32:7E:13:55:8D:DC:3B:6A:79:B3:5D:32`，
存在 repo secrets：`SMELT_ANDROID_KEYSTORE_BASE64`、`SMELT_ANDROID_KEYSTORE_PASSWORD`、`SMELT_ANDROID_KEY_PASSWORD`、
`SMELT_ANDROID_KEY_ALIAS`。原件与密码由维护者离线保管，不进仓库。

本地出正式签名包：照 `mobile/README.md` 的 Release signing 写好 `mobile/android/key.properties`，再

```sh
cd mobile && flutter build apk --release --target-platform android-arm64 \
  --build-name 2026.1023.0 --build-number <比线上大的数>
```

iOS 没有发版流水线：免费开发者账号只能直装到自己的设备（`scripts/install-ios-profile.sh`）。
