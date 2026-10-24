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
| 2. CLI для Windows | готово до тестування: [docs/02-stage2-windows-cli.md](docs/02-stage2-windows-cli.md) |
| 3. CLI для macOS (Core Audio tap / ScreenCaptureKit) | далі |
| 4–7. Надійність, трей, інсталятори, завантаження на сервер | заплановано |

## Збірка

Потрібні Rust (stable) і CMake. На Windows також Visual Studio Build Tools з компонентом «Desktop development with C++».

```bash
cargo build --release -p recorder-cli
```

Бінарник з'явиться в `target/release/recorder` (`recorder.exe` на Windows).

Тести (у release-режимі, бо симуляції довгі):

```bash
cargo test --workspace --release
```

## Використання

```bash
recorder devices                  # список пристроїв
recorder start                    # запис; Ctrl+C — стоп
recorder start --process Zoom.exe # Windows: лише звук Zoom
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
crates/recorder-capture  мікрофон (cpal), WASAPI loopback; macOS — етап 3
crates/recorder-cli      консольна програма `recorder`
```

Ліцензії сторонніх компонентів: [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
