# Доменный контракт (Domain Contract)

Документ содержит единственные авторитарные определения всех доменных концепций, сущностей и типов данных системы. Все остальные контракты и компоненты обязаны ссылаться на определения из этого документа без их повторного или альтернативного переопределения.

## 1. Базовые доменные типы

### 1.1. `ActionKind` (Тип действия)
Перечисление поддерживаемых системой принудительных действий:
* `BlockInternet` — принудительное ограничение доступа к сети Интернет для целевого пользователя Windows.
* `ShutdownComputer` — принудительное завершение работы операционной системы и выключение компьютера.

### 1.2. `TimerId` (Идентификатор таймера)
Уникальный непрозрачный идентификатор запланированного действия (например, UUID v4 или криптографически стойкий 128-битный идентификатор). Служит для адресации операций отмены, обновления и логирования конкретного таймера.

### 1.3. `Deadline` (Дедлайн)
Абсолютный момент времени в шкале UTC (`UtcDateTime` / Unix Timestamp в миллисекундах), в который запланированное действие ДОЛЖНО быть приведено в исполнение. Использование локального смещения времени или относительных счетчиков в качестве канонического дедлайна ЗАПРЕЩЕНО.

### 1.4. `WarningThreshold` (Порог предупреждения)
Строго фиксированное дискретное перечисление временных интервалов до наступления `Deadline`, на которых система формирует предупреждение:
* `M60` — ровно за 60 минут (3600 секунд) до дедлайна;
* `M30` — ровно за 30 минут (1800 секунд) до дедлайна;
* `M20` — ровно за 20 минут (1200 секунд) до дедлайна;
* `M10` — ровно за 10 минут (600 секунд) до дедлайна;
* `M3`  — ровно за 3 минуты (180 секунд) до дедлайна.

### 1.5. `ScheduledAction` (Запланированное действие)
Структура, представляющая активное или запланированное системное действие:
* `id: TimerId` — уникальный идентификатор;
* `action_kind: ActionKind` — тип действия (`BlockInternet` или `ShutdownComputer`);
* `deadline: Deadline` — абсолютное время исполнения;
* `created_at: UtcDateTime` — время постановки таймера;
* `created_by: Initiator` — инициатор действия (`ParentTelegram(UserId)`, `ParentLocalPin`, `SystemPolicy`);
* `emitted_thresholds: Set<WarningThreshold>` — множество порогов, уведомления по которым уже были эмитированы (для предотвращения дубликатов).

### 1.6. `WarningEvent` (Событие предупреждения)
Событие, генерируемое `core` при достижении порога предупреждения:
* `timer_id: TimerId` — идентификатор таймера;
* `action_kind: ActionKind` — тип запланированного действия;
* `threshold: WarningThreshold` — достигнутый порог;
* `deadline: Deadline` — целевой дедлайн;
* `emitted_at: UtcDateTime` — метка времени генерации события.

### 1.7. `InternetState` (Состояние интернет-шлюза)
Текущее состояние доступа целевого пользователя к сети Интернет:
* `Unrestricted` — доступ не ограничен (трафик разрешен);
* `Blocked` — доступ принудительно заблокирован для SID целевого пользователя.

### 1.8. `ShutdownState` (Состояние питания системы)
Текущее состояние процесса выключения компьютера:
* `Idle` — таймер выключения не активен;
* `Scheduled` — выключение запланировано на определенный `Deadline`;
* `InProgress` — инициирован системный вызов завершения работы ОС Windows.

### 1.9. `ChatMessage` (Сообщение чата)
Сущность текстового сообщения между родителем и ребенком:
* `id: MessageId` — уникальный идентификатор сообщения;
* `sender: MessageSender` — отправитель (`Parent` или `Child`);
* `text: String` — текстовое содержимое (валидированная UTF-8 строка ограниченной длины);
* `timestamp: UtcDateTime` — время отправки;
* `delivery_status: DeliveryStatus` — статус доставки (`Pending`, `DeliveredToService`, `DeliveredToTelegram`, `DeliveredToTray`, `Failed`).

### 1.10. `ServiceHealth` (Состояние работоспособности службы)
Диагностическое состояние службы:
* `status: HealthStatus` (`Healthy`, `Degraded`, `Critical`);
* `uptime_seconds: u64` — время непрерывной работы службы;
* `internet_gate_healthy: bool` — исправность драйвера/подсистемы фильтрации;
* `persistence_healthy: bool` — доступность и целостность базы данных/файла состояния;
* `telegram_connected: bool` — статус связи с Telegram API;
* `active_tray_sessions: u32` — количество подключенных клиентов `tray`.

## 2. Алгебраические типы взаимодействия (Commands & Events)

### 2.1. `Command` (Команды управления)
Строго типизированные намерения на изменение состояния системы.

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

    // Авторизация и сессия
    VerifyPin { pin_attempt: SensitivePinString },
    QueryStatus,
}
```

### 2.2. `Event` (Доменные события)
Строго типизированные факты, произошедшие в системе:

```rust
// Концептуальное представление типа Event
enum Event {
    InternetStateChanged { previous: InternetState, current: InternetState, reason: StateChangeReason },
    ShutdownStateChanged { previous: ShutdownState, current: ShutdownState },
    TimerScheduled { action: ScheduledAction },
    TimerCancelled { id: TimerId, action_kind: ActionKind },
    TimerExpired { id: TimerId, action_kind: ActionKind },
    WarningThresholdReached { event: WarningEvent },
    ChatMessageReceived { message: ChatMessage },
    PinAuthenticationResult { success: bool, lock_timeout_seconds: Option<u32> },
    ServiceHealthUpdated { health: ServiceHealth },
}
```
