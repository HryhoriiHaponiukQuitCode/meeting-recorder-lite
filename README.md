# Meeting Recorder

Легка програма для запису аудіо онлайн-дзвінків (Zoom, Meet, Teams, Telegram, браузер) і їхньої **локальної транскрипції**: хто що сказав, із часом.

- **Лівий канал:** мікрофон (ви).
- **Правий канал:** системний звук (співрозмовники).
- **Формат:** Ogg Opus, 16 кГц, ~32 кбіт/с (≈ 8–15 МБ на годину).
- **Один дзвінок — один файл** `part_0001.ogg`, навіть після паузи чи сну комп'ютера. Ділити на частини можна лише в консольній версії: `recorder start --part-minutes 10`.
- **Для прослуховування** поруч пишеться `listen.ogg`: обидві сторони змішані в моно, тож чути всіх в обох вухах (≈ 5–8 МБ на годину). Для транскрипції беріть `part_*.ogg`, де канали розділені.
- **Транскрипція на цьому комп'ютері:** `transcript.txt` (діалог з часом: хто говорить — ім'я або «Participant N»), `transcript.srt` (субтитри до `listen.ogg`) і `transcript.json`. Аудіо нікуди не надсилається.
- **Стійкість до збоїв:** якщо програма впаде, втрачається не більше ~1,5 с запису.

> **Юридичне нагадування.** Ви відповідаєте за отримання згоди всіх учасників розмови на запис відповідно до законодавства (GDPR, місцеві закони).

## Стан

| Етап | Стан |
|---|---|
| 1. Дослідження й архітектура | готово: [docs/01-research-and-architecture.md](docs/01-research-and-architecture.md) |
| 2. CLI для Windows | готово: [docs/02-stage2-windows-cli.md](docs/02-stage2-windows-cli.md) |
| 3–6. macOS, надійність, вікно програми, інсталятори | готово: [docs/03-stages-3-6.md](docs/03-stages-3-6.md) |
| Локальна транскрипція | готово на macOS, Windows — лише збірка в CI: [docs/04-transcription.md](docs/04-transcription.md) |
| 7. Завантаження на сервер | пізніше |

## Встановлення

Готові інсталятори є в артефактах [GitHub Actions](../../actions) (`meeting-recorder-macos-arm64`, `meeting-recorder-windows-x64`):

- **macOS 13+:** `Meeting Recorder_<версія>_aarch64.dmg`. Перетягніть програму в «Програми»; перший запуск — правий клік → «Відкрити».
- **Windows 10/11:** `meeting-recorder_<версія>_x64-setup.exe`. Встановлюється для поточного користувача, права адміністратора не потрібні.

Відкрийте **Meeting Recorder** з «Програм» (macOS) або меню «Пуск» (Windows): **Start recording → Stop → Show in Finder** (інтерфейс англійською).

## Транскрипція

1. **Settings → Transcription → Download speech models.** Один раз. Перервану закачку можна продовжити, файли перевіряються за SHA256.
   - macOS, якість **Best** (за замовчуванням): ≈ 1,4 ГБ завантаження, ≈ 1,5 ГБ на диску.
   - Якість **Fast** і Windows: 490 МБ завантаження, 670 МБ на диску.
2. Після кожного запису транскрипція запускається сама (вимикається перемикачем «Transcribe automatically»). Або натисніть **Transcribe** у картці «Last recording».
3. **Вікно з трьох частин.** Ліворуч навігація: **Record**, **Recordings**, внизу **Settings**. Посередині вибраний розділ. Праворуч розшифровка вибраного запису: час, хто говорить (у кожного свій колір), текст, який можна виділяти й копіювати. Є пошук з підсвіткою («Find in transcript»), **Copy all** і **Open file** (`transcript.txt`). На старті нічого не відкрито: запис вибираєш ти.
4. **Recordings** — список записів по 20 на сторінці (**Newer** / **Older**; читаються лише записи поточної сторінки): дата, тривалість, стан транскрипції. Клік по будь-якому місцю рядка показує розшифровку праворуч. Біля запису є **Transcribe** (кілька записів стають у чергу), прогрес зі **Stop** і кнопка з папкою, яка показує файли у Finder / Explorer.

Як це працює:

- **Fast** (і єдиний режим на Windows): NVIDIA Parakeet TDT 0.6B v3 (INT8) + silero VAD через sherpa-onnx. Українська, англійська й ще 23 європейські мови, мова визначається сама.
- **Best** (лише macOS): текст пише Whisper large-v3-turbo (whisper.cpp на графічному процесорі, Metal), а Parakeet заповнює те, що Whisper пропустив, і дає англійські фрази. Слова, які Whisper «вигадав» без мовлення, відкидаються. Помітно менше помилок у словах і термінах, але година дзвінка займає ≈ 10–20 хв і до 2 ГБ пам'яті.
- Канали розпізнаються окремо, тож «Me» і «Others» не плутаються. Якщо мікрофон чув співрозмовників із динаміків, ці фрази позначаються як луна й не показуються.
- **Голоси співрозмовників** розрізняються за тембром. Голосові відбитки 3D-Speaker CAM++ (27 МБ) на мовленнєвих вікнах системного звуку, кластеризація. Додає ≈ 1 хв на годину дзвінка. Вимикається перемикачем «Tell voices apart». Дуже короткі репліки («так», «ок») можуть потрапити не тому голосу.
- **Імена.** Кожен учасник (твій мікрофон і кожен голос) підписаний ім'ям, якщо розмова його однозначно показує: «Каті, розкажи…» → наступна, хто говорить, — Катя; «Дякую, Роман, за коментар» → Роман той, хто щойно говорив; «мене звати…», «I'm…». Інакше — «Participant 1», «Participant 2»… у порядку появи. Імена беруться зі словника українських та англійських імен у будь-якому відмінку й можуть бути помилковими.
- Коли Mac гріється, транскрипція робить перерви, а коли гарячий — чекає, доки охолоне (у вікні: «resting» / «cooling down»). Текст від цього не змінюється.
- Транскрипція працює в окремому процесі з низьким пріоритетом. Збій чи брак пам'яті там не зачепить запис. Коли починається новий запис, транскрипція зупиняється й потім продовжується з місця зупинки.
- Швидкість Fast (Mac M4, «Speed: Gentle», 2 потоки): година дзвінка ≈ 2 хв. Пам'ять ≈ 1,5 ГБ під час роботи, потім звільняється. «Speed: Fast» (4 потоки) приблизно на чверть швидший.

## Збірка

Потрібні Rust (stable) і CMake. На Windows також Visual Studio Build Tools з компонентом «Desktop development with C++» і Git Bash.

Один раз зберіть sherpa-onnx (статично, без синтезу мовлення; ≈ 5 хв, на половині ядер з низьким пріоритетом):

```bash
scripts/build-sherpa-onnx.sh
```

Далі:

```bash
cargo build --release -p recorder-cli -p recorder-tray -p recorder-transcriber
```

- `target/release/meeting-recorder` — програма з вікном.
- `target/release/meeting-recorder-transcribe` — транскрибувальник (вікно й консоль запускають його самі; має лежати поруч).
- `target/release/recorder` — консольна версія.
- На Windows файли мають розширення `.exe`.

Інсталятор (`.app` + `.dmg` або NSIS `.exe`) з'явиться в `dist/`. Потрібен [cargo-packager](https://github.com/crabnebula-dev/cargo-packager):

```bash
cargo install cargo-packager --version 0.11.8 --locked
cd crates/recorder-tray && cargo packager --release
```

Тести (у release-режимі, бо симуляції довгі):

```bash
cargo test --workspace --release
```

Тести з моделлю (вимкнені за замовчуванням, потрібні модель і тестові записи) описані в `crates/recorder-transcriber/tests/model.rs`.

## Консольна версія

На macOS консольна версія з термінала не отримає дозволу на системний звук, тож для запису користуйтеся застосунком.

```bash
recorder devices                  # список пристроїв
recorder start                    # запис; Ctrl+C — стоп
recorder start --process Zoom.exe # Windows: лише звук Zoom
recorder start --aec on           # придушення луни завжди (auto — лише для динаміків)
recorder verify "<папка сесії>" --wav   # перевірка + me.wav / them.wav
recorder recover                  # закрити сесії після збою
recorder model download           # завантажити моделі (також status, verify, delete)
recorder transcribe "<папка сесії>"     # транскрипція; --threads 4 — швидше, --quality fast|best, --no-voices
```

| | Записи | Логи | Модель |
|---|---|---|---|
| Windows | `Documents\Meeting Recorder\recordings\` | `%LOCALAPPDATA%\Meeting Recorder\logs\` | `%LOCALAPPDATA%\Meeting Recorder\models\` |
| macOS | `~/Documents/Meeting Recorder/recordings/` | `~/Library/Logs/Meeting Recorder/` | `~/Library/Application Support/Meeting Recorder/models/` |

## Структура

```
crates/recorder-core         часова шкала, дрейф, Opus/Ogg, частини, manifest, відновлення (без ОС-залежностей)
crates/recorder-capture      мікрофон (cpal), WASAPI loopback, Core Audio tap, ScreenCaptureKit, дозволи, сон
crates/recorder-app          сесія запису в окремому потоці, повідомлення, налаштування, логи, запуск транскрипції
crates/recorder-cli          консольна програма `recorder`
crates/recorder-tray         програма з вікном `meeting-recorder` (egui)
crates/recorder-transcribe   транскрипція без нативного коду: модель, шматки, діалог, луна, файли, протокол
crates/recorder-transcriber  транскрибувальник `meeting-recorder-transcribe` (sherpa-onnx + ONNX Runtime)
scripts/                     збірка sherpa-onnx
packaging/                   Info.plist, entitlements, іконки, ліцензії
```

Ліцензії сторонніх компонентів: [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
