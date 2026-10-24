# Етап 1. Дослідження і пропозиція архітектури

Дата: 2026-10-24. Статус: **чекаю підтвердження** (рішення, які треба підтвердити, — у розділі 9).

---

## 1. Коротко

- Усі три референсні проєкти мають ліцензію **MIT**, тож запозичувати можна (з copyright-повідомленнями).
- Жоден із них не робить того, що потрібно нам, «з коробки»: усі пишуть **моно** окремі доріжки для локальної ASR, ні в кого немає компенсації дрейфу годинників, і в кожному є дрібні дефекти (нижче). Запозичуємо **схеми виклику ОС API** і перевірені рішення, а не модулі цілком.
- **UI:** чистий трей на `tray-icon` + `muda` + `tao` (без WebView). Це найлегший варіант: ~4–6 МБ бінарник, ~15–25 МБ RAM.
- **Формат:** Ogg Opus, 16 кГц, 2 канали, але кодуються як **два незалежні моно-потоки (Opus multistream)**, а не як звичайне стерео. Для ffmpeg файл звичайний 2-канальний, а канали гарантовано не змішуються (пояснення в розділі 5).
- **Синхронізація:** обидва джерела прив'язуються до одного монотонного годинника ОС (`mach_absolute_time` / QPC) через адаптивний ресемплер з PI-регулятором. Діри (тиша в WASAPI loopback, зміна пристрою, простій process tap) **автоматично заповнюються тишею**, бо записувач бере звук з часової шкали, а не з черги пакетів.

---

## 2. Що знайдено в референсних проєктах

| Проєкт | Ліцензія | Коміт, який вивчав |
|---|---|---|
| Zackriya-Solutions/meeting-minutes (Meetily) | MIT, © 2024 Zackriya Solutions | `a2cb62e` (2026-09-10) |
| romeroej2/OpenCassava | MIT, © 2026 OpenCassava contributors | `f71f902` (2026-06-04) |
| minutiae-dev/minutiae | MIT, © 2026 Minutiae contributors (+ `THIRD_PARTY.md`: залежності MIT/Apache/ISC, моделі під своїми ліцензіями, нам не потрібні) | `811bd19` (2026-09-06) |

### 2.1 minutiae: Core Audio process tap (macOS)

Файл `engine/Sources/EngineCore/Capture/SystemAudioTap.swift` (Swift, сам портований з `insidegui/AudioCap`).

**Беремо (портуємо на Rust через `cidre`):**
- Ланцюжок `CATapDescription(stereoGlobalTapButExcludeProcesses: [self])` → `AudioHardwareCreateProcessTap` → приватний aggregate device з `TapList` і `kAudioSubTapDriftCompensationKey = true` → `AudioDeviceCreateIOProcIDWithBlock` → `AudioDeviceStart`.
- Виключення **власного процесу** з tap (через `kAudioHardwarePropertyTranslatePIDToProcessObject`).
- Формат tap **читається** з `kAudioTapPropertyFormat`, ніколи не припускається.
- Розпізнавання відмови в дозволі: `OSStatus 'nope'` (0x6E6F7065).
- **Перепідключення при зміні виводу:** listener на `kAudioHardwarePropertyDefaultOutputDevice`, tap перебудовується під новий пристрій, а формат для споживача лишається тим самим на всю сесію.
- Час буфера з `inInputTime.mHostTime` (з фолбеком на `inNow`).
- З `AudioFileWriter.swift` — ідея **вирівнювання за часом сесії**: якщо джерело мовчить (IOProc tap не викликається, коли вивід простоює), записувач досипає тишу. Поріг розриву порівнюється між сусідніми буферами (0,25 с), тому дрейф не накопичується у фальшиві паузи. Також вибір 16 кГц / 16 kbps на канал для Opus, з виміряними цифрами: 16 кГц дешевше і менше, ніж 24 кГц.

**Не беремо:** Swift-рушій і IPC-сайдкар (у нас один Rust-процес), ASR/LLM, CAF-контейнер (він не стійкий до збою, і тому вони паралельно пишуть WAV; у нас Ogg сам стійкий до збою). Windows-частина в minutiae — лише перелік пристроїв, loopback там не реалізовано.

### 2.2 Meetily: захоплення на macOS через `cidre` і моніторинг пристроїв

Файли: `frontend/src-tauri/src/audio/capture/core_audio.rs`, `device_monitor.rs`, `devices/platform/*.rs`, `permissions.rs`.

**Беремо:**
- Готові Rust-виклики `cidre` для process tap: `ca::TapDesc`, `ca::AggregateDevice::with_desc`, `create_io_proc_id`, `ca::device_start`. Це той самий ланцюжок, що й у minutiae, але вже на Rust.
- Важливе спостереження з коментарів: якщо в aggregate покласти **і** вихідний пристрій, **і** tap, звук дублюється (луна). Тому в aggregate кладемо тільки tap.
- Важливе спостереження: якщо дозволу немає, tap може віддавати **цифрову тишу** замість помилки. Тому потрібна окрема перевірка дозволу (див. 4.9).
- `device_monitor.rs`: listener Core Audio на зміну списку пристроїв + більший поріг «відключено» для Bluetooth (HFP-перемикання триває кілька секунд).

**Не беремо / виправляємо:** у realtime-колбеку tap береться `Mutex` (так у RT-потоці не можна, буде заїкання при навантаженні); 128K ring buffer із «завершенням після 10 переповнень»; моно-tap. Решта (Whisper/Parakeet, LLM, БД, Next.js UI) нам не потрібна.

### 2.3 OpenCassava: WASAPI loopback (Windows) і ехоподавлення

Файли: `opencassava/src-tauri/src/audio_windows.rs`, `crates/opencassava-core/src/audio/echo_cancel.rs`, `audio_macos.rs` (ScreenCaptureKit).

**Беремо:**
- Скелет WASAPI loopback: `IMMDeviceEnumerator` → `GetDefaultAudioEndpoint(eRender, eConsole)` → `IAudioClient::Initialize(SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, …, GetMixFormat())` → `IAudioCaptureClient`; пошук пристрою за friendly name.
- **AEC** (`echo_cancel.rs`, 687 рядків разом із тестами): частотний адаптивний фільтр із розбиттям на блоки (PBFDAF/NLMS, 16 × 256 семплів, хвіст ~256 мс при 16 кГц), на `realfft`. Є тести на чисту луну, на одночасну мову (double-talk), на швидкість збіжності й затриману луну. Це хороша основа для «простого ехоподавлення» з ТЗ.
- Фолбек на macOS 13: ScreenCaptureKit, лише аудіо.

**Виправляємо:**
- Опитування `sleep(10ms)` і **один пакет за ітерацію**. При 10-мс пакетах це відставання, а при навантаженні втрати. У нас: event-driven режим і цикл, доки `GetNextPacketSize() > 0`.
- Ігнорують `AUDCLNT_BUFFERFLAGS_SILENT` (там у буфері сміття, а треба нулі) і `DATA_DISCONTINUITY`.
- **Тишу в loopback не вставляють**: правий канал «з'їжджає». Це підводний камінь №1 із ТЗ.
- Лінійний ресемплер **без стану між чанками**: на межах чанків клацання, і накопичується помилка округлення довжини. У нас `rubato`.
- У SCK-фолбеку non-interleaved буфери (окремий буфер на канал) склеюються і потім діляться як interleaved. Для стерео це неправильно.

### 2.4 Висновок

Чистий новий репозиторій. Портуємо **схеми викликів** (tap, aggregate, loopback, listeners, AEC-алгоритм із тестами) з посиланням на джерело в заголовку кожного файлу і в `THIRD_PARTY_NOTICES.md`. Синхронізацію, дрейф, нарізку, формат і стійкість до збою пишемо самі: ні в кого з трьох цього немає в потрібному вигляді.

---

## 3. Стек і версії (перевірено на crates.io 2026-10-24)

| Що | Crate | Версія | Ліцензія | Примітка |
|---|---|---|---|---|
| Мікрофон (обидві ОС) | `cpal` | 0.18.2 | Apache-2.0 | дає `capture` timestamp (QPC / mHostTime) |
| Loopback Windows | `wasapi` | 0.26.0 | MIT | є `new_application_loopback_client(pid, include_tree)` для process loopback |
| WinAPI (сповіщення пристроїв, power) | `windows` | 0.62.x | MIT/Apache | |
| macOS: process tap + ScreenCaptureKit + AVFoundation | `cidre` | 0.29.0 | MIT | один crate на обидва шляхи; використовується в Meetily |
| Ресемплінг + корекція дрейфу | `rubato` | 5.0.1 | MIT/Apache | `Async`-ресемплер з `set_resample_ratio_relative` |
| Opus | `opus` | 0.4.0 | MIT/Apache | збирає libopus (BSD) статично через `opusic-sys`; **є `MSEncoder` (multistream)** |
| Ogg | `ogg` | 0.9.2 | BSD-3 | `PacketWriter`, ручний контроль кінця сторінки |
| Lock-free SPSC | `ringbuf` | 0.5.3 | MIT/Apache | RT-безпечна передача з колбеків |
| Трей | `tray-icon` / `muda` / `tao` | 0.26.1 / 0.21.2 / 0.37.2 | MIT/Apache | |
| Нативні діалоги | `rfd` | 0.17.2 | MIT | нагадування про згоду, помилки дозволів |
| FFT для AEC | `realfft` | 3.5.0 | MIT/Apache | |
| Логи | `tracing` + `tracing-appender` | 0.1.44 / 0.2.5 | MIT | файл із ротацією |
| Пакування | `cargo-packager` | 0.11.8 | MIT/Apache | `.app`/`.dmg`/`.msi`/NSIS без Tauri |
| Інше | `serde_json`, `uuid`, `chrono`, `clap`, `ctrlc`, `dirs` | актуальні | MIT/Apache | |

Альтернатива для AEC, яку варто оцінити на етапі 4: `aec3` 0.4.0 (MIT/BSD-3, порт WebRTC AEC3 на чистому Rust). `webrtc-audio-processing` відкидаю: важка C++-збірка, плюс кілька МБ до інсталятора.

### Tauri чи `tray-icon`: обираю `tray-icon`

| | `tray-icon` + `tao` + `rfd` | Tauri 2 + vanilla HTML |
|---|---|---|
| RAM | ~15–25 МБ | +30–80 МБ на процес WebView2 / WKWebView |
| Інсталятор | ~4–6 МБ | ~6–10 МБ; на Win10 може знадобитися WebView2 bootstrapper |
| Що потрібно з UI | меню, змінна іконка, таймер, 2 індикатори рівня, 1 діалог | те саме |

Усе, що вимагає ТЗ, робиться нативно:
- **Індикатори рівня — прямо в іконці трею**: дві мініатюрні смужки (L = мікрофон, R = система), іконка перемальовується 5 разів на секунду (RGBA 32×32 генеруємо в коді).
- Червона крапка під час запису.
- Таймер: у `title` поруч з іконкою на macOS, у підказці (tooltip) на Windows.
- Дублюючий пункт меню «🎙 ▮▮▮▯▯ 🔊 ▮▯▯▯▯».
- Нагадування про згоду: нативний `rfd`-діалог із прапорцем «більше не показувати».

---

## 4. Архітектура

### 4.1 Потоки даних

```
 [Мікрофон: cpal]──RT колбек──►SPSC ring──┐
                                           │      ┌────────────────────── DSP-потік ─────────────────────┐
 [Система: WASAPI loopback /               ├─────►│ per source: downmix→mono, rubato Async → 16 кГц,     │
  CoreAudio tap / SCK]──RT колбек─►SPSC ring┘      │ PI-регулятор дрейфу за host-часом, розміщення на     │
   (кожен блок: host_time + кадри)                 │ спільній часовій шкалі сесії; рівні (RMS/peak);      │
                                                   │ [опц.] AEC: L -= echo(R)                              │
                                                   └───────────────┬───────────────────────────────────────┘
                                                                   │ кожні 20 мс нижче «горизонту фіксації»
                                                                   ▼
                                       Interleaver: L,R (відсутнє = нулі) → Opus MSEncoder (2×mono, VOIP)
                                                                   │ пакети
                                                                   ▼
                                  IO-потік: Ogg PacketWriter → part_NNNN.ogg; кінець сторінки + write
                                  + fdatasync щосекунди; ротація кожні 10 хв; manifest.json (атомарно)
```

- **RT-колбеки** нічого не алокують, не блокуються і не логують. Вони тільки копіюють семпли і host-timestamp у lock-free SPSC-буфер (~2 с ємності). Якщо буфер переповнився, рахуємо відкинуті кадри в атомарному лічильнику, і DSP-потік пише це в лог.
- **DSP+encode** — один потік, прокидається кожні ~20 мс. **IO** — окремий потік, щоб `fsync` (іноді десятки мс) не гальмував обробку.

### 4.2 Часова шкала сесії і дрейф (підводні камені 1, 2, 3)

- Еталонний годинник: монотонний годинник ОС. `T0` — початок сесії. Позиція семплу на шкалі: `n = (t − T0) × 16000`.
- Кожне джерело: `rubato::Async` (полиноміальна інтерполяція — дешево, для мови достатньо) з **PI-регулятором**. Помилка — це різниця між «де блок має лежати за timestamp» і «скільки ми вже записали» (timestamp згладжений, бо джитер колбеків кілька мс). Регулятор підкручує `resample_ratio_relative` у межах ±0,5%.
- Якщо помилка більша за **±40 мс** (WASAPI мовчить, перепідключили пристрій, tap простоював), регулятор не тягне повільно, а робимо **жорстку ресинхронізацію**: вставляємо нулі або відкидаємо кадри.
- Через це обидва канали прив'язані до одного годинника незалежно від тактових генераторів пристроїв. Очікувана розбіжність ≤ ~10 мс у сталому режимі і < 40 мс у найгіршому випадку. Ціль < 100 мс/год виконується із запасом.
- **Горизонт фіксації:** записувач кодує тільки ту частину шкали, що старша за `now − 400 мс` (запас на найповільніші колбеки: Bluetooth, SCK). Там, де джерело нічого не дало, лежать нулі. Тому тиша в WASAPI loopback (№1) і паузи при зміні пристрою (№4) заповнюються **автоматично**, без окремих хаків.
- Якщо дані запізнилися понад горизонт, вони відкидаються з записом у лог.

### 4.3 Кодування і файли

- `opus::MSEncoder`: 16 кГц, 2 канали, **2 потоки, 0 coupled**, mapping family 1, `OPUS_APPLICATION_VOIP`, VBR, 16 kbps на потік (32 сумарно), фрейм 20 мс (320 семплів), complexity 5.
- Ogg: `OpusHead` (pre-skip = lookahead енкодера), `OpusTags` (vendor, `ENCODER=meeting-recorder x.y.z`, `CHANNEL_0=mic`, `CHANNEL_1=system`).
- **Сторінку закриваємо кожні 50 пакетів (1 с)**, потім `write` + `fdatasync`. Втрати при збої ≤ 0,4 с (горизонт) + 1 с (сторінка) ≈ 1,4 с. Ціль ≤ 2 с виконується.
- **Ротація кожні 10 хв = рівно 30 000 пакетів.** Межа частин лежить на точному індексі семплу шкали. На кожну частину новий енкодер, правильний pre-skip, granule position останньої сторінки задає точне обрізання хвоста. Кожен семпл шкали потрапляє рівно в одну частину. Автотест декодує частини і порівнює кількість і вміст семплів із вхідними.
- Розмір: 32 kbps ≈ 14,4 МБ/год + ~1% накладних витрат Ogg. На тиші VBR дає менше.

### 4.4 manifest.json

Записується атомарно (tmp → `fsync` → `rename`). Оновлюється при старті, при кожній ротації частини, при подіях (зміна пристрою, сон) і при зупинці.

```json
{
  "schema_version": 1,
  "session_id": "5b0c…",
  "status": "recording | completed | crashed_recovered",
  "started_at_utc": "2026-10-24T14:30:05.123Z",
  "ended_at_utc": null,
  "app_version": "0.1.0",
  "os": { "family": "macos", "version": "15.1", "arch": "aarch64" },
  "audio": {
    "codec": "opus", "container": "ogg", "sample_rate": 16000, "channels": 2,
    "channel_map": { "L": "mic", "R": "system" },
    "opus_layout": "multistream_2x_mono", "bitrate_bps": 32000, "frame_ms": 20
  },
  "devices": {
    "mic": "MacBook Pro Microphone",
    "output": "MacBook Pro Speakers",
    "output_kind": "speakers | headphones | bluetooth | unknown",
    "echo_risk": "likely | unlikely | unknown",
    "aec": "off | on"
  },
  "system_capture": "coreaudio_process_tap | screencapturekit | wasapi_loopback | wasapi_process_loopback",
  "part_length_s": 600,
  "parts": [
    { "file": "part_0001.ogg", "start_offset_ms": 0, "duration_ms": 600000, "status": "closed" },
    { "file": "part_0002.ogg", "start_offset_ms": 600000, "duration_ms": 41240, "status": "open" }
  ],
  "events": [
    { "t_ms": 312400, "type": "device_change", "source": "mic", "from": "…", "to": "AirPods Pro" },
    { "t_ms": 1800000, "type": "sleep", "duration_ms": 95000 }
  ]
}
```

### 4.5 Відновлення після збою

- При старті програма бере **lock-файл сесії** (`std::fs::File::try_lock`), щоб не «відновлювати» сесію, яку ще пише живий процес.
- Знаходить сесії зі статусом `recording` і для кожної:
  1. сканує останню частину посторінково з перевіркою CRC;
  2. обрізає незавершений хвіст до останньої цілої сторінки;
  3. рахує тривалість за останньою granule position;
  4. дописує `duration_ms`, `ended_at_utc` (час модифікації файлу) і `status = crashed_recovered`.
- Сесію не можна пошкодити повторно: усі операції ідемпотентні.

### 4.6 Зміна пристроїв (№4)

- Режим за замовчуванням: «слідувати за системним пристроєм за замовчуванням»; опційно в конфігу можна закріпити конкретний пристрій.
- **Windows:** `IMMNotificationClient::OnDefaultDeviceChanged` / `OnDeviceStateChanged`, а для мікрофона ще й error-колбек `cpal`. Після події перебудовуємо потік.
- **macOS:** listeners на `DefaultInputDevice`, `DefaultOutputDevice` (перебудова tap за схемою minutiae), `NominalSampleRate` (HFP міняє частоту на 16/24 кГц) і `DeviceIsAlive`.
- Поки пристрій перепідключається, шкала заповнюється нулями (4.2). Ресемплер створюється заново під нову частоту. Подія потрапляє в `manifest.events` і в лог.
- Перепідключення з експоненційною затримкою (0,2 → 5 с). **Запис ніколи не зупиняється через пристрій.**

### 4.7 Колонки замість навушників (№5)

- **Мінімум (етап 2–3):** визначаємо тип виводу і пишемо в manifest `output_kind` і `echo_risk`.
  - macOS: `kAudioDevicePropertyTransportType` + `DataSource` (`ispk` / `hdpn`).
  - Windows: `PKEY_AudioEndpoint_FormFactor`.
- **Бажане (етап 4):** порт AEC з OpenCassava на мікрофонний канал із системним каналом як референсом. Наша шкала вже вирівнює обидва канали за часом, тому фільтру лишається компенсувати тільки акустичну і апаратну затримку (десятки мс, в межах його 256 мс). Режим `aec = auto | on | off`; `auto` вмикає AEC, лише коли `echo_risk = likely`. На навушниках сирий мікрофон не чіпаємо.
- **Страховка на сервері** (на майбутнє): відкидати сегменти «Я», текст яких збігається з одночасним сегментом «Співрозмовник».

### 4.8 Сон і блокування (№7)

- **Під час запису** забороняємо *idle sleep*:
  - macOS: `IOPMAssertionCreateWithName(PreventUserIdleSystemSleep)`;
  - Windows: `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)`.

  Блокування екрана запис не зупиняє: дзвінок теж іде далі.
- **Примусовий сон** (кришка ноутбука, меню «Сон»):
  - macOS: `NSWorkspaceWillSleep` / `DidWake`;
  - Windows: `WM_POWERBROADCAST` (`PBT_APMSUSPEND` / `PBT_APMRESUMEAUTOMATIC`).

  Перед сном закриваємо поточну частину, після пробудження перепідключаємо пристрої, відкриваємо нову частину і пишемо подію `sleep` з тривалістю. **Час сну тишею не заповнюємо**, бо це могли б бути години. Розрив видно з `start_offset_ms` частин.

### 4.9 Дозволи macOS (№6)

- **Мікрофон:** `AVCaptureDevice.authorizationStatus` / `requestAccess` через `cidre::av`. Опис береться з `NSMicrophoneUsageDescription`.
- **Системний звук (tap):**
  - опис береться з `NSAudioCaptureUsageDescription`;
  - відмова розпізнається за `'nope'` під час створення tap;
  - додатково перевіряємо дозвіл заздалегідь через `TCCAccessPreflight(kTCCServiceAudioCapture)` (приватний TCC.framework, підхід AudioCap; для поширення поза App Store це допустимо). Причина: tap може тихо віддавати нулі (див. 2.2).
- **ScreenCaptureKit:** дозвіл «Запис екрана», перевірка через `CGPreflightScreenCaptureAccess`.
- При відмові показуємо зрозуміле повідомлення з кнопкою, що відкриває потрібний розділ System Settings (`x-apple.systempreferences:…`).
- **Для CLI-прототипу:** TCC читає Info.plist бінарника. Тому вбудовуємо Info.plist у секцію `__TEXT,__info_plist` через `-sectcreate` у `build.rs`. Інакше дозволи прив'язуються до Terminal.app, а описи не показуються.
- **Підпис і нотаризація:** готуємо `entitlements.plist` (hardened runtime + `com.apple.security.device.audio-input`) і конфіг `cargo-packager`. Збірку без підпису це не блокує.

### 4.10 Системний звук: вибір шляху на macOS

| macOS | Шлях | Дозвіл |
|---|---|---|
| ≥ 14.4 | Core Audio process tap (global, без власного процесу) | «System Audio Recording» |
| 13.0 – 14.3 | ScreenCaptureKit, лише аудіо (`excludesCurrentProcessAudio`, мінімальний відеопотік 2×2 @ 1 fps, який ігноруємо) | «Запис екрана» |

У ТЗ вказано 14.2+. Але обидві перевірені реалізації (AudioCap і minutiae) вимагають **14.4+**: у 14.2–14.3 API вже є, але поведінка з дозволами інша. Пропоную межу 14.4 (див. рішення 9.3).

**Ризик лінкування:** `cidre` оголошує `AudioHardwareCreateProcessTap` як звичайний (strong) extern. На macOS 13 dyld може не запустити програму («symbol not found»). Рішення: `-weak_framework CoreAudio` або власна обгортка через `dlsym`. Перевірю на етапі 3 (потрібна машина або VM з macOS 13).

### 4.11 Windows-специфіка

- **Loopback:** `wasapi` в event-driven режимі на пристрої виводу за замовчуванням. Поки `GetNextPacketSize() > 0`, читаємо всі пакети. Позиції беремо з `u64QPCPosition`. `SILENT` → нулі. `DATA_DISCONTINUITY` → лог і ресинхронізація за QPC.
- **Process loopback** (Win10 2004+ / Win11): `--process Zoom.exe` → PID → `new_application_loopback_client(pid, include_tree = true)`. Формат задаємо явно, бо `GetMixFormat` тут не працює. За замовчуванням — увесь системний звук.
- **Мікрофон:** `cpal` (WASAPI shared). Якщо «Доступ до мікрофона для класичних програм» вимкнено, буде `E_ACCESSDENIED`. Ловимо його і показуємо зрозуміле повідомлення.

### 4.12 Логи, конфіг, шляхи

| | macOS | Windows |
|---|---|---|
| Записи | `~/Documents/Meeting Recorder/recordings/` | `%USERPROFILE%\Documents\Meeting Recorder\recordings\` |
| Логи | `~/Library/Logs/Meeting Recorder/` | `%LOCALAPPDATA%\Meeting Recorder\logs\` |
| Конфіг | `~/Library/Application Support/Meeting Recorder/config.toml` | `%APPDATA%\Meeting Recorder\config.toml` |

- Логи: `tracing` із щоденною ротацією, 7 файлів. Пишемо:
  - старт: ОС, пристрої, формати, вибраний шлях захоплення;
  - щохвилини: рівні, дрейф у ppm, поточна розбіжність каналів, відкинуті кадри, CPU і RSS;
  - усі події пристроїв і дозволів.
- Попередження «Не чути співрозмовників»: якщо правий канал мовчить > 2 хв, а лівий активний, з'являється сповіщення в треї. Це проти найчастішої проблеми «записався тільки я».

---

## 5. Чому multistream, а не звичайне Opus-стерео

У звичайному стерео (coupled) енкодер кодує канали спільно. SILK-режим використовує mid/side-прогноз і на низьких бітрейтах може **звужувати стерео аж до моно**, якщо вважає, що різниця між каналами неважлива. Для двох незалежних голосів це означає **перетікання голосу між каналами**, а сервер транскрибує канали окремо.

Multistream із 2 незалежними моно-потоками (RFC 7845, mapping family 1, `stream_count = 2`, `coupled_count = 0`) кодує кожен канал окремо, по 16 kbps. Розмір той самий, ізоляція гарантована. Для ffmpeg і whisper це звичайний 2-канальний Ogg Opus, тож команди `pan=mono|c0=c0` / `c0=c1` з ТЗ працюють без змін. **Перевірю на етапі 2:** згенерований файл має пройти саме ці команди з розділу «Серверна частина».

Окреме спостереження щодо `ffmpeg -f concat … -c copy`: на кожній межі частин можливий дубль ~6,5 мс (lookahead енкодера): concat-демуксер може не застосувати pre-skip наступної частини. Це ≤ 0,04 с на годину, однаково для обох каналів, тому на синхрон каналів не впливає. На етапі 2 виміряю і, якщо треба, запропоную точнішу серверну команду (декодувати частини окремо, потім склеїти WAV).

---

## 6. Структура репозиторію

```
meeting-recorder/
├─ Cargo.toml                     # workspace
├─ crates/
│  ├─ recorder-core/              # без платформних залежностей, повністю покрито тестами
│  │  └─ src/ timeline.rs  drift.rs  resample.rs  interleave.rs
│  │          encoder.rs (Opus MS + Ogg)  part_writer.rs  manifest.rs
│  │          recovery.rs  levels.rs  aec.rs (етап 4)  config.rs  session.rs
│  ├─ recorder-capture/           # трейт AudioSource + реалізації
│  │  └─ src/ mic_cpal.rs
│  │          windows/ loopback.rs  process_loopback.rs  device_notify.rs  power.rs
│  │          macos/   process_tap.rs  sck.rs  device_listener.rs  permissions.rs  power.rs
│  ├─ recorder-cli/               # bin `recorder`: start | recover | devices | probe
│  └─ recorder-tray/              # bin трей-програми (етап 5)
├─ packaging/
│  ├─ macos/ Info.plist  entitlements.plist
│  └─ windows/ (WiX / NSIS шаблони)
├─ tools/
│  ├─ verify_parts.sh|ps1         # ffprobe/ffmpeg: читабельність, тривалість, канали, команди з ТЗ
│  └─ sync_check.py               # вимірює розсинхрон L/R за тест-сигналом
├─ docs/  ARCHITECTURE.md  TESTING.md  01-research-and-architecture.md
├─ .github/workflows/ ci.yml  release.yml
├─ THIRD_PARTY_NOTICES.md  README.md  LICENSE
```

- `recorder-core` — це вся логіка надійності (етап 4). Тести працюють на будь-якій ОС у CI без аудіопристроїв: синтетичні джерела з дрейфом ±300 ppm, рваними колбеками, мовчанням, зміною частоти посеред запису.
- `recorder-upload` (tus) додамо на етапі 7. Архітектура вже готова: частини закриті й незмінні, а manifest — джерело правди для черги завантаження.

---

## 7. Як перевірятимемо

- **Автотести (етап 4):**
  - дрейф ±300 ppm за 3 год синтетичного звуку дає розсинхрон < 20 мс;
  - межі частин: декодована сума частин побітово збігається з вхідною шкалою (з урахуванням кодека — за кількістю семплів і кореляцією);
  - тиша в loopback: розриви 0,1 с – 10 хв;
  - відновлення: обрізаний посеред сторінки файл → `crashed_recovered`, тривалість правильна;
  - ресемплінг 44,1 / 48 / 16 / 24 кГц;
  - тести AEC з OpenCassava.
- **Ручна перевірка розсинхрону (на ваших машинах):** `recorder probe-sync` грає короткі «клацання» в колонки раз на 10 с протягом години, а мікрофон їх чує. `tools/sync_check.py` порівнює час клацань у L і R на початку і в кінці.
- **Критерії ТЗ:** `tools/verify_parts` проганяє `ffmpeg concat` + `pan` + (опційно) `whisper-cli` на `part_0001.ogg`, плюс CPU/RSS із логів.

---

## 8. Ризики і невідомі

| Ризик | Що робимо |
|---|---|
| macOS 13: strong-лінкування символів tap у `cidre` | `-weak_framework` / `dlsym`; тест на macOS 13 |
| Поведінка tap на 14.2–14.3 | межа 14.4 (рішення 9.3) |
| `TCCAccessPreflight` — приватне API | тільки для перевірки; фолбек — евристика «цифрові нулі N секунд при активному виводі» |
| Bluetooth HFP: частота й пристрій змінюються кілька разів за секунди | debounce подій 500 мс + жорстка ресинхронізація шкали |
| Windows-збірку не можна запустити на цьому Mac | збірка через GitHub Actions (`windows-latest`) з артефактом `.exe`; запуск і тест на вашій Windows-машині |
| AEC може зачепити ваш голос під час одночасної мови | `auto`, тільки на колонках; тести double-talk; можна вимкнути |
| На цьому Mac **не встановлено Rust** | для етапу 3 треба `rustup` (встановлю з вашого дозволу) |

---

## 9. Рішення, які треба підтвердити

1. **UI = `tray-icon`** (без WebView); індикатори рівня — в іконці трею і в меню.
2. **Opus multistream 2×mono** замість coupled-стерео (той самий розмір, гарантована ізоляція; для ffmpeg звичайне стерео).
3. **macOS: process tap з 14.4** (не з 14.2), для 13.0–14.3 — ScreenCaptureKit.
4. **Сон:** заборона idle-сну під час запису; при примусовому сні — нова частина і подія в manifest, без заповнення тишею.
5. **AEC:** на етапі 4, режим `auto` (тільки коли вивід = колонки).
6. **Організаційне:**
   - назва програми / bundle id (робоча: «Meeting Recorder», `com.<ваш-домен>.meeting-recorder`);
   - чи є GitHub-репозиторій, куди пушити (потрібен для Windows-збірок через Actions на етапі 2);
   - чи можна встановити Rust через `rustup` на цьому Mac.

## 10. План етапу 2 (після підтвердження)

1. `git init`, workspace, `recorder-core`: шкала, ресемплер, енкодер, нарізка, manifest (з юніт-тестами для енкодера і нарізки вже тут).
2. `recorder-capture/windows/loopback.rs` + `mic_cpal.rs`.
3. `recorder-cli`: `recorder start [--out DIR] [--part-minutes N] [--process Zoom.exe]`, Ctrl+C → коректне закриття; `recorder devices`.
4. CI на `windows-latest` → артефакт `recorder.exe`.
5. Інструкція: як запустити, що перевірити (ffprobe, команди з ТЗ, тест 15-хв дзвінка), які логи надіслати.
