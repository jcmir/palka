# Доменный контракт (Domain Contract)

Документ содержит единственные авторитарные определения всех доменных концепций, сущностей и типов данных системы. Все остальные контракты и компоненты обязаны ссылаться на определения из этого документа без их повторного или альтернативного переопределения.

## 1. Базовые доменные типы

### 1.1. `ActionKind` (Тип действия)
Перечисление поддерживаемых системой принудительных действий:
* `BlockInternet` — принудительное ограничение доступа к сети Интернет для сконфигурированного пользователя Windows.
* `ShutdownComputer` — принудительное завершение работы операционной системы и выключение компьютера.

### 1.2. `TimerId` (Идентификатор таймера)
Уникальный непрозрачный идентификатор запланированного действия (128-битный идентификатор или UUID v4). Служит для однозначной адресации операций отмены, обновления и аудита конкретного таймера.

### 1.3. `Deadline` (Дедлайн)
Абсолютный момент времени в шкале UTC (`UtcDateTime` / Unix Timestamp в миллисекундах), в который запланированное действие ДОЛЖНО быть приведено в исполнение. Использование локального смещения времени или относительных счетчиков в качестве канонического дедлайна ЗАПРЕЩЕНО.

### 1.4. `WarningThreshold` (Порог предупреждения)
Строго фиксированное дискретное перечисление временных интервалов до наступления `Deadline`, на которых формируются предупреждения:
* `M60` — ровно за 60 минут (3600 секунд) до дедлайна;
* `M30` — ровно за 30 минут (1800 секунд) до дедлайна;
* `M20` — ровно за 20 минут (1200 секунд) до дедлайна;
* `M10` — ровно за 10 минут (600 секунд) до дедлайна;
* `M3`  — ровно за 3 минуты (180 секунд) до дедлайна.

### 1.5. `Initiator` (Инициатор действия)
Субъект, запросивший изменение состояния системы:
* `ParentTelegram { user_id: u64 }` — авторизованный родитель через Telegram;
* `ParentLocalPin` — родитель через локальный ввод PIN-кода в GUI/трее.

### 1.6. `ActionExecutionState` (Состояние исполнения действия)
Жизненный цикл выполнения запланированного действия:
* `Pending` — действие запланировано и ожидает наступления дедлайна (персистентное нетерминальное состояние);
* `Executing` — дедлайн наступил или получена немедленная команда, операция передана на исполнение в платформенный слой (персистентное нетерминальное состояние для защиты от сбоев во время вызова);
* `Completed` — действие успешно исполнено и подтверждено платформенным шлюзом (терминальное состояние, таймер удаляется из активного набора);
* `Failed { reason: String }` — попытка исполнения завершилась ошибкой платформенного слоя (персистентное состояние, подлежащее повторным попыткам с задержкой);
* `Missed` — дедлайн наступил во время отключенного состояния системы для действия `ShutdownComputer` (терминальное состояние, таймер аннулируется, отправляется уведомление).

### 1.7. `ScheduledAction` (Запланированное действие)
Структура, представляющая активное или обрабатываемое системное действие:
* `id: TimerId` — уникальный идентификатор;
* `action_kind: ActionKind` — тип действия (`BlockInternet` или `ShutdownComputer`);
* `deadline: Deadline` — абсолютное время дедлайна (UTC);
* `created_at: UtcDateTime` — время постановки таймера;
* `created_by: Initiator` — авторизованный инициатор;
* `emitted_thresholds: Set<WarningThreshold>` — множество порогов, уведомления по которым уже были эмитированы или отмечены пройденными;
* `execution_state: ActionExecutionState` — текущее состояние исполнения.

### 1.8. `WarningEvent` (Событие предупреждения)
Событие, вычисляемое `core` при пересечении очередного порога предупреждения:
* `timer_id: TimerId` — идентификатор таймера;
* `action_kind: ActionKind` — тип запланированного действия;
* `threshold: WarningThreshold` — достигнутый порог;
* `deadline: Deadline` — целевой дедлайн;
* `emitted_at: UtcDateTime` — метка времени генерации события.

### 1.9. `DesiredInternetState` и `InternetState` (Желаемое и Наблюдаемое состояние сети)
Система строго разделяет целевую политику службы и физически подтвержденное состояние сетевого шлюза:
* **`DesiredInternetState` (Целевая политика)** — авторитарное намерение службы:
  * `Unrestricted` — политика свободного доступа;
  * `Blocked` — политика принудительной блокировки.
* **`InternetState` (Наблюдаемое состояние шлюза)** — фактически подтвержденное платформой состояние:
  * `Unrestricted` — доступ физически разрешен шлюзом;
  * `Blocked` — доступ физически заблокирован шлюзом для целевого SID;
  * `Unknown` — состояние шлюза не удалось подтвердить из-за ошибки драйвера/платформы.

### 1.10. `ShutdownState` (Состояние питания системы)
Волатильное состояние процесса выключения компьютера во время работы текущей сессии ОС:
* `Idle` — выключение не запланировано и не выполняется;
* `Scheduled` — выключение запланировано на определенный `Deadline`;
* `InProgress` — системный вызов завершения работы передан в ОС Windows в момент наступления дедлайна.

### 1.11. `StateChangeReason` (Причина изменения состояния)
Причина, вызвавшая переход сетевого или системного состояния:
* `TimerExpired { timer_id: TimerId }` — наступление дедлайна таймера;
* `ImmediateCommand { initiator: Initiator }` — немедленная команда блокировки;
* `ManualRestore { initiator: Initiator }` — ручная команда снятия блокировки;
* `StartupRestoration` — восстановление желаемой политики при старте службы;
* `PlatformSync` — периодическая синхронизация или реакция на сбой платформы.

### 1.12. `SensitivePinString` (Защищенная строка PIN-кода)
Обертка над строковым представлением PIN-кода, гарантирующая очистку памяти после использования (zeroize), запрет логирования и запрет неконтролируемой сериализации.

### 1.13. `MessageId` и `MessageSender`
* `MessageId` — уникальный 128-битный идентификатор текстового сообщения.
* `MessageSender` — отправитель сообщения: `Parent` или `Child`.

### 1.14. `DeliveryStatus` (Статус доставки сообщения)
Статус продвижения сообщения по транспортным узлам:
* `Pending` — сообщение сформировано клиентом;
* `AcceptedByService` — принято службой и сохранено в персистентную очередь;
* `AcceptedByTelegram` — подтверждено сервером Telegram Bot API (HTTP 200 OK / acknowledgement);
* `DeliveredToTray` — успешно передано по IPC в активное окно трея ребенка;
* `Failed { reason: String }` — ошибка доставки на одном из этапов.

### 1.15. `HealthStatus` и `ServiceHealth` (Здоровье службы)
* **`HealthStatus`**:
  * `Healthy` — все компоненты и адаптеры функционируют штатно;
  * `Degraded` — возникли некритические сбои (недоступен Telegram, временная ошибка применения правил шлюза в режиме повторов);
  * `Critical` — критический сбой подсистем безопасности (повреждение базы данных, отказ низкоуровневых драйверов Windows).
* **`ServiceHealth`**:
  * `status: HealthStatus` — интегральный статус здоровья;
  * `uptime_seconds: u64` — время непрерывной работы процесса службы;
  * `internet_gate_healthy: bool` — исправность подсистемы фильтрации;
  * `persistence_healthy: bool` — целостность файлов состояния;
  * `telegram_connected: bool` — доступность Telegram API;
  * `active_tray_sessions: u32` — число подключенных IPC-клиентов;
  * `last_error: Option<String>` — текст последней зарегистрированной ошибки.

### 1.16. `ServiceLifecycleStage` (Стадии жизненного цикла службы)
* `ServiceStarted` — процесс службы Windows запущен, управление передано функции инициализации рантайма;
* `ServiceReady` — инициализация завершена (персистентное состояние загружено, дескрипторы безопасности настроены, попытка применения `DesiredInternetState` выполнена, служба готова к обработке команд).

### 1.17. `StatusSnapshot` (Снимок состояния системы)
Агрегированный снимок состояния, формируемый службой для клиентов:
* `desired_internet_state: DesiredInternetState` — целевая политика сети;
* `observed_internet_state: InternetState` — фактически подтвержденное состояние шлюза;
* `shutdown_state: ShutdownState` — статус управления питанием;
* `active_actions: Vec<ScheduledAction>` — список активных таймеров;
* `health: ServiceHealth` — диагностика здоровья службы;
* `target_child_sid: String` — сконфигурированный SID ребенка;
* `timestamp: UtcDateTime` — метка времени снимка (UTC).

## 2. Недоменные платформенные типы (Non-Domain Platform Types)
Следующие типы принадлежат платформенному слою (`windows-platform`) и определяются в соответствующих платформенных контрактах:
* `SecurityIdentifier` — бинарный идентификатор безопасности Windows SID (определен в платформенном слое Windows);
* `GateError` — тип ошибки шлюза фильтрации (определен в [docs/005-internet-gate-contract.md](./005-internet-gate-contract.md));
* `PowerError` — тип ошибки подсистемы питания (определен в [docs/006-power-contract.md](./006-power-contract.md)).

## 3. Алгебраические типы взаимодействия (Commands & Events)

### 3.1. `Command` (Доменные команды управления)
```rust
// Концептуальное представление типа Command
enum Command {
    // Управление Интернетом
    ScheduleInternetBlock { duration_minutes: u32, initiator: Initiator },
    CancelInternetBlockTimer { initiator: Initiator },
    RestoreInternet { initiator: Initiator },
    ImmediateInternetBlock { initiator: Initiator },

    // Управление питанием
    ScheduleShutdown { duration_minutes: u32, initiator: Initiator },
    CancelShutdownTimer { initiator: Initiator },

    // Чат
    SendChildMessage { text: String },
    SendParentMessage { text: String },

    // Проверка аутентификации и запрос статуса
    VerifyPin { pin_attempt: SensitivePinString },
    QueryStatus,
}
```

*Примечание*: Запрос подписки на транспортный поток событий IPC (`SubscribeEvents`) является низкоуровневой операцией управления транспортом IPC, а не доменной командой `Command` (см. [Контракт IPC](./004-ipc-contract.md)).

### 3.2. `Event` (Доменные события)
```rust
// Концептуальное представление типа Event
enum Event {
    InternetPolicyChanged { desired: DesiredInternetState, observed: InternetState, reason: StateChangeReason },
    ShutdownStateChanged { previous: ShutdownState, current: ShutdownState },
    TimerScheduled { action: ScheduledAction },
    TimerCancelled { id: TimerId, action_kind: ActionKind },
    TimerExpired { id: TimerId, action_kind: ActionKind },
    WarningThresholdReached { event: WarningEvent },
    MissedDeadlineOccurred { action: ScheduledAction, reason: String },
    ChatMessageReceived { message: ChatMessage },
    PinAuthenticationResult { success: bool, lock_timeout_seconds: Option<u32> },
    ServiceHealthUpdated { health: ServiceHealth },
    ServiceLifecycleEvent { stage: ServiceLifecycleStage },
}
```
