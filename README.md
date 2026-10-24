# Meeting Recorder

Легка програма для запису аудіо онлайн-дзвінків (Zoom, Meet, Teams, Telegram, браузер) для подальшої транскрипції на сервері через whisper.cpp.

- **Лівий канал:** мікрофон (ви).
- **Правий канал:** системний звук (співрозмовники).
- **Формат:** Ogg Opus, 16 кГц, ~32 кбіт/с (≈ 8–15 МБ на годину).
- **Запис ділиться на частини** по 10 хв: `part_0001.ogg`, `part_0002.ogg`, …
- **Стійкість до збоїв:** якщо програма впаде, втрачається не більше ~1,5 с запису.

> **Юридичне нагадування.** Ви відповідаєте за отримання згоди всіх учасників розмови на запис відповідно до законодавства (GDPR, місцеві закони).

## Стан

| Етап | Стан |
|---|---|
| 1. Дослідження й архітектура | готово: [docs/01-research-and-architecture.md](docs/01-research-and-architecture.md) |
| 2. CLI для Windows | готово: [docs/02-stage2-windows-cli.md](docs/02-stage2-windows-cli.md) |
| 3–6. macOS, надійність, трей, інсталятори | готово: [docs/03-stages-3-6.md](docs/03-stages-3-6.md) |
| 7. Завантаження на сервер | пізніше |

## Встановлення

Готові інсталятори є в артефактах [GitHub Actions](../../actions) (`meeting-recorder-macos-arm64`, `meeting-recorder-windows-x64`):

- **macOS 13+:** `Meeting Recorder_<версія>_aarch64.dmg`. Перетягніть програму в «Програми»; перший запуск — правий клік → «Відкрити».
- **Windows 10/11:** `Meeting Recorder_<версія>_x64-setup.exe`. Встановлюється для поточного користувача, права адміністратора не потрібні.

Програма живе в рядку меню (macOS) або в треї (Windows): **Почати запис → Зупинити запис → Відкрити папку записів**.

## Збірка

Потрібні Rust (stable) і CMake. На Windows також Visual Studio Build Tools з компонентом «Desktop development with C++».

```bash
cargo build --release -p recorder-cli -p recorder-tray
```

- `target/release/meeting-recorder` — трей-застосунок.
- `target/release/recorder` — консольна версія.
- На Windows обидва файли мають розширення `.exe`.

Інсталятор (`.app` + `.dmg` або NSIS `.exe`) з'явиться в `dist/`. Потрібен [cargo-packager](https://github.com/crabnebula-dev/cargo-packager):

```bash
cargo install cargo-packager --version 0.11.8 --locked
cd crates/recorder-tray && cargo packager --release
```

Тести (у release-режимі, бо симуляції довгі):

```bash
cargo test --workspace --release
```

## Консольна версія

На macOS консольна версія з термінала не отримає дозволу на системний звук, тож користуйтеся застосунком.

```bash
recorder devices                  # список пристроїв
recorder start                    # запис; Ctrl+C — стоп
recorder start --process Zoom.exe # Windows: лише звук Zoom
recorder start --aec on           # придушення луни завжди (auto — лише для динаміків)
recorder verify "<папка сесії>" --wav   # перевірка + me.wav / them.wav
recorder recover                  # закрити сесії після збою
```

| | Записи | Логи |
|---|---|---|
| Windows | `Documents\Meeting Recorder\recordings\` | `%LOCALAPPDATA%\Meeting Recorder\logs\` |
| macOS | `~/Documents/Meeting Recorder/recordings/` | `~/Library/Logs/Meeting Recorder/` |

## Структура

```
crates/recorder-core     часова шкала, дрейф, Opus/Ogg, частини, manifest, відновлення (без ОС-залежностей)
crates/recorder-capture  мікрофон (cpal), WASAPI loopback, Core Audio tap, ScreenCaptureKit, дозволи, сон
crates/recorder-app      сесія запису в окремому потоці, повідомлення, налаштування, логи
crates/recorder-cli      консольна програма `recorder`
crates/recorder-tray     трей-застосунок `meeting-recorder`
packaging/               Info.plist, entitlements, іконки
```

Ліцензії сторонніх компонентів: [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
