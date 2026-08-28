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

### 1.6. `ScheduledAction` (Запланированное действие)
Структура, представляющая активное или обрабатываемое системное действие:
* `id: TimerId` — уникальный идентификатор;
* `action_kind: ActionKind` — тип действия (`BlockInternet` или `ShutdownComputer`);
* `deadline: Deadline` — абсолютное время дедлайна (UTC);
* `created_at: UtcDateTime` — время постановки таймера;
* `created_by: Initiator` — авторизованный инициатор;
* `emitted_thresholds: Set<WarningThreshold>` — множество порогов, уведомления по которым уже были эмитированы (для исключения дубликатов);
* `execution_state: ActionExecutionState` — текущее состояние исполнения (`Pending`, `Executing`, `Completed`, `Failed { reason: String }`, `Missed`).

### 1.7. `WarningEvent` (Событие предупреждения)
Событие, вычисляемое `core` при достижении очередного порога предупреждения:
* `timer_id: TimerId` — идентификатор таймера;
* `action_kind: ActionKind` — тип запланированного действия;
* `threshold: WarningThreshold` — достигнутый порог;
* `deadline: Deadline` — целевой дедлайн;
* `emitted_at: UtcDateTime` — метка времени генерации события.

### 1.8. `InternetState` (Состояние интернет-шлюза)
Текущий авторитарный режим доступа целевого пользователя к сети Интернет:
* `Unrestricted` — доступ не ограничен (трафик разрешен);
* `Blocked` — доступ принудительно заблокирован для SID целевого пользователя.

### 1.9. `ShutdownState` (Состояние питания системы)
Волатильное состояние процесса выключения компьютера во время работы текущей сессии ОС:
* `Idle` — выключение не запланировано и не выполняется;
* `Scheduled` — выключение запланировано на определенный `Deadline`;
* `InProgress` — системный вызов завершения работы ОС Windows передан в ОС.

### 1.10. `ChatMessage` (Сообщение чата)
Сущность текстового сообщения между родителем и ребенком:
* `id: MessageId` — уникальный идентификатор сообщения;
* `sender: MessageSender` (`Parent` или `Child`);
* `text: String` — валидированная строка текста (ограниченного размера);
* `timestamp: UtcDateTime` — время отправки;
* `delivery_status: DeliveryStatus` (`Pending`, `DeliveredToService`, `DeliveredToTelegram`, `DeliveredToTray`, `Failed`).

### 1.11. `ServiceHealth` (Состояние работоспособности службы)
Диагностическое состояние службы:
* `status: HealthStatus` (`Healthy`, `Degraded`, `Critical`);
* `uptime_seconds: u64` — время непрерывной работы службы;
* `internet_gate_healthy: bool` — исправность подсистемы фильтрации трафика;
* `persistence_healthy: bool` — доступность и целостность файлов состояния;
* `telegram_connected: bool` — статус связи с Telegram API;
* `active_tray_sessions: u32` — количество подключенных клиентов `tray`;
* `last_error: Option<String>` — описание последней зарегистрированной ошибки.

### 1.12. `StatusSnapshot` (Снимок состояния системы)
Полный агрегированный снимок состояния, формируемый службой для отправки клиентам:
* `internet_state: InternetState` — текущий режим фильтрации сети;
* `shutdown_state: ShutdownState` — текущий статус управления питанием;
* `active_actions: Vec<ScheduledAction>` — перечень активных таймеров;
* `health: ServiceHealth` — диагностическое состояние службы;
* `target_child_sid: String` — строковое представление сконфигурированного SID ребенка;
* `timestamp: UtcDateTime` — время формирования снимка.

## 2. Алгебраические типы взаимодействия (Commands & Events)

### 2.1. `Command` (Доменные команды управления)
Строго типизированные намерения на изменение доменного состояния:

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

*Примечание по протоколу*: Запрос подписки на транспортный поток событий IPC (`SubscribeEvents`) является низкоуровневой операцией управления транспортом IPC, а не доменной командой `Command` (см. [Контракт IPC](./004-ipc-contract.md)).

### 2.2. `Event` (Доменные события)
Строго типизированные факты, зарегистрированные в системе:

```rust
// Концептуальное представление типа Event
enum Event {
    InternetStateChanged { previous: InternetState, current: InternetState, reason: StateChangeReason },
    ShutdownStateChanged { previous: ShutdownState, current: ShutdownState },
    TimerScheduled { action: ScheduledAction },
    TimerCancelled { id: TimerId, action_kind: ActionKind },
    TimerExpired { id: TimerId, action_kind: ActionKind },
    WarningThresholdReached { event: WarningEvent },
    MissedDeadlineOccurred { action: ScheduledAction, reason: String },
    ChatMessageReceived { message: ChatMessage },
    PinAuthenticationResult { success: bool, lock_timeout_seconds: Option<u32> },
    ServiceHealthUpdated { health: ServiceHealth },
    ServiceLifecycleEvent { stage: ServiceLifecycleStage }, // ServiceStarted, ServiceReady
}
```
