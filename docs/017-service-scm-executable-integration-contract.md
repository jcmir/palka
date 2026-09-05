# Нормативный контракт интеграции исполняемого файла службы (Service SCM Executable Integration Contract V1)

Настоящий документ определяет нормативные требования к интеграции исполняемого файла службы `palka-service` (`crates/service/src/main.rs`) с диспетчером Windows Service Control Manager (SCM), процедурой начальной инициализации (`bootstrap_service()`), долгоживущим координатором рантайма (`ServiceRuntime`) и системным циклом координированной остановки (`graceful teardown`).

---

## 1. Назначение и границы ответственности (Purpose & Scope)

### 1.1. Область ответственности контракта
Жизненный цикл **`SERVICE-SCM-EXECUTABLE-INTEGRATION`** является связующим звеном верхнего уровня процесса службы и владеет исключительно:
1. Точкой входа процесса службы `crates/service/src/main.rs` (композиционный корень, Composition Root);
2. Входом в системный диспетчер служб Windows SCM (`run_palka_service_dispatcher`);
3. Реализацией точки входа сервиса (`PalkaServiceEntry = fn(ScmServiceContext)`), вызываемой диспетчером при запуске службы;
4. Публикацией промежуточных состояний запуска (`SERVICE_START_PENDING`) с монотонно возрастающими контрольными точками (чекпоинтами);
5. Вызовом процедуры начальной валидации и подготовки состояния через абстракцию начальной загрузки (`ServiceBootstrapPort`);
6. Отложенным конструированием производственных зависимостей рантайма строго после завершения Checkpoint 2 и вызовом `ServiceRuntime::start(...) -> Result<Self, ServiceRuntimeError>`;
7. Инспекцией готовности рантайма через аксессор `runtime.readiness() -> &StartupReadiness` и трансляцией результата в статус SCM (`SERVICE_RUNNING` при `StartupReadiness::Ready(_)` или `StartupReadiness::Degraded(_)`, либо фазово-согласованным переходом в `SERVICE_STOPPED` с системным кодом ошибки при сбое старта);
8. Ожиданием и обработкой системных управляющих сигналов `SCM_RUNTIME_DELIVERED_CONTROLS` (`SERVICE_CONTROL_STOP` и `SERVICE_CONTROL_SHUTDOWN`); сигнал `SERVICE_CONTROL_INTERROGATE` полностью обрабатывается внутренне адаптером `scm_runtime` и рантайму не передается (`INTERROGATE_OWNER: palka-windows-platform::scm_runtime`, `INTERROGATE_DELIVERED_TO_RUNTIME: NO`);
9. Переводом службы в состояние `SERVICE_STOP_PENDING` и инициированием координированной остановки (`ServiceRuntime::stop()`);
10. Финальной публикацией `SERVICE_STOPPED` с корректным системным кодом выхода Win32 (`dwWin32ExitCode`); гарантируется строго не более одной успешной публикации (AT MOST ONE SUCCESSFUL SERVICE_STOPPED PUBLICATION);
11. Интеграцией с защитным механизмом аварийного перехвата паник и раннего возврата (`ActiveServiceFallback` с системным кодом `ERROR_EXCEPTION_IN_SERVICE` (1064)).

### 1.2. Области, категорически исключенные из контракта
Данный контракт **КАТЕГОРИЧЕСКИ НЕ ИМЕЕТ ПРАВА**:
* Реализовывать логику сетевой фильтрации WFP (`Fwpm*`) или инспектировать сетевые пакеты;
* Выполнять прямые низкоуровневые вызовы управления питанием Windows (`InitiateSystemShutdownExW`, `ExitWindowsEx`);
* Реализовывать сетевой HTTP-клиент или протокол взаимодействия с Telegram Bot API;
* Реализовывать сервер или клиент именованных каналов IPC (`NamedPipe`);
* Реализовывать графический интерфейс системного трея (`palka-tray`);
* Реализовывать установщик, инсталляционные сценарии MSI/WiX или регистрацию автозапуска;
* Создавать собственные правила переходов доменных состояний или таймеров в обход `palka-core`.

---

## 2. Архитектурный инвариант (Architectural Invariant)

Интеграция исполняемого файла строго подчиняется общесистемному архитектурному контракту PALKA:

```text
CORE DECIDES.
SERVICE ENFORCES.
PLATFORM EXECUTES.
TRAY DISPLAYS.
TELEGRAM REQUESTS.
RUNTIME SERIALIZES AUTHORITATIVE MUTATION.
```

Слой интеграции исполняемого файла представляет собой тонкий **оркестрационный клей** (Orchestration Seam) между операционной системой Windows и рантаймом PALKA. Он не дублирует функции координатора, не владеет доменными автоматами и не производит несогласованных побочных эффектов.

Управляющие сигналы SCM разделяются по ответственности:
* `SCM_RUNTIME_DELIVERED_CONTROLS`: сигналы `STOP` и `SHUTDOWN` передаются в канал управления службы.
* `INTERROGATE_OWNER`: обработка сигнала `SERVICE_CONTROL_INTERROGATE` выполняется адаптером `palka-windows-platform::scm_runtime` (возвращает `NO_ERROR`), сигнал рантайму не передается (`INTERROGATE_DELIVERED_TO_RUNTIME: NO`).

---

## 3. Точный жизненный цикл исполняемого файла (Executable Lifecycle)

### 3.1. Нормативная последовательность запуска и остановки
Канонический жизненный цикл процесса `palka-service.exe` состоит из следующих строго упорядоченных фаз:

```text
[Процесс palka-service.exe запущен (SCM / SCM Dispatcher Thread)]
                            │
                            ▼
         main() вызывает run_palka_service_dispatcher(palka_service_entry)
                            │
                            ▼
               Вход в ServiceMain callback
                            │
                            ▼
     [Фаза 1] Публикация SERVICE_START_PENDING (Checkpoint 1, WaitHint 30 000 мс)
                            │
                            ▼
     [Фаза 2] Вызов bootstrap.bootstrap()
              ├── Ошибка: Переход START_PENDING ──► SERVICE_STOPPED (ERROR_EXCEPTION_IN_SERVICE) ──► Выход
              └── Успех: Получен BootstrappedServiceState
                            │
                            ▼
     [Фаза 3] Публикация SERVICE_START_PENDING (Checkpoint 2, WaitHint 30 000 мс)
                            │
                            ▼
     [Фаза 4] Отложенное конструирование производственных зависимостей и вызов
              runtime_factory.start(bootstrapped):
              let runtime = ServiceRuntime::start(bootstrapped, gate, power, clock, id_source, retry_policy)?;
              ├── Ошибка конструктора (Err(ServiceRuntimeError)) / Panic:
              │   Переход START_PENDING ──► SERVICE_STOPPED (ERROR_EXCEPTION_IN_SERVICE / 1064) ──► Выход
              └── Успех: Получен экземпляр ServiceRuntime
                            │
                            ▼
     [Фаза 5] Инспекция готовности: let readiness = runtime.readiness();
              ├── StartupReadiness::Ready(_) ──┐
              └── StartupReadiness::Degraded(_) ─┴─► [Фаза 6] Публикация SERVICE_RUNNING
                                                     (dwControlsAccepted = STOP | SHUTDOWN)
                                                              │
                                                              ▼
     [Фаза 7] Ожидание управляющего сигнала (lifecycle.wait_for_control())
              Получен SERVICE_CONTROL_STOP либо SERVICE_CONTROL_SHUTDOWN
                                                              │
                                                              ▼
     [Фаза 8] Публикация SERVICE_STOP_PENDING (Checkpoint 1, WaitHint 15 000 мс)
                                                              │
                                                              ▼
     [Фаза 9] Остановка рантайма: runtime.stop() (graceful teardown, джойн воркеров)
              ├── Успех остановки: dwWin32ExitCode = NO_ERROR (0)
              └── Ошибка остановки: dwWin32ExitCode = ERROR_EXCEPTION_IN_SERVICE (1064)
                                                              │
                                                              ▼
     [Фаза 10] Публикация SERVICE_STOPPED (dwWin32ExitCode)
```

### 3.2. Неделимость контрольных точек запуска
1. **Чекпоинт 1**: Публикуется до начала дискового ввода-вывода Bootstrap. Гарантирует SCM, что процесс принял управление и не завис на этапе загрузки библиотек.
2. **Чекпоинт 2**: Публикуется сразу после успешного завершения `bootstrap.bootstrap()`, строго перед конструированием производственных адаптеров рантайма, процедурой Startup Recovery и первичным согласованием шлюза (`RUNTIME_FACTORY_DEFERRED_UNTIL_AFTER_CP2: YES`).
3. Категорически запрещается переходить в `SERVICE_RUNNING` сразу после Фазы 2. Успешный Bootstrap признается необходимым, но **недостаточным** условием готовности службы (`BOOT-12`).
4. Доклад `SERVICE_RUNNING` разрешен строго после завершения восстановления рантайма и подтверждения его готовности (`runtime.readiness()` возвращает `StartupReadiness::Ready(_)` или `StartupReadiness::Degraded(_)` при наличии реальных производственных адаптеров). Фатальный сбой старта выражается как `ServiceRuntime::start(...) -> Err(...)` (возврат ошибки `Err(ServiceRuntimeError)`).

---

## 4. Шлюз производственных зависимостей (Critical Production Dependency Gate)

### 4.1. Формулировка проблемы
Фактическая сигнатура производственного конструктора `ServiceRuntime::start(...)` в `crates/service/src/runtime.rs`:
```rust
pub fn start<G, P, C, I, R>(
    bootstrapped: BootstrappedServiceState,
    gate: G,
    power: P,
    clock: C,
    id_source: I,
    retry_policy: R,
) -> Result<Self, ServiceRuntimeError>
where
    G: InternetGate + 'static,
    P: PowerController + 'static,
    C: RuntimeClock + 'static,
    I: IdSource + 'static,
    R: InternetRetryPolicy + 'static;
```

После успешного создания готовность рантайма получается через метод-аксессор:
```rust
pub fn readiness(&self) -> &StartupReadiness;
```

Тип `StartupReadiness` определен в `crates/service/src/runtime.rs` строго из двух вариантов:
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupReadiness {
    Ready(StatusSnapshot),
    Degraded(StatusSnapshot),
}
```

На текущий момент в репозитории:
* `SystemClock` (`C`) — **РЕАЛИЗОВАН** и доступен в составе `palka-service`;
* `InternetGate` (`G`) — реальная производственная WFP-реализация **ОТСУТСТВУЕТ**;
* `PowerController` (`P`) — реальная производственная Win32 Power-реализация **ОТСУТСТВУЕТ**;
* `IdSource` (`I`) — конкретная производственная реализация **НЕ СФОРМИРОВАНА**;
* `InternetRetryPolicy` (`R`) — производственные параметры **НЕ СПЕЦИФИЦИРОВАНЫ**.

### 4.2. Нормативный выбор стратегии: СТРАТЕГИЯ A (DEPENDENCY-FIRST)
Настоящий контракт безальтернативно утверждает **СТРАТЕГИЮ A (DEPENDENCY-FIRST)**:

```text
PRODUCTION_DEPENDENCY_STRATEGY:
A_DEPENDENCY_FIRST
```

> [!IMPORTANT]
> **Принцип Стратегии A**:
> Реализация исполняемого файла `crates/service/src/main.rs` и его сквозная сборка в исполняемый бинарник службы **ЗАБЛОКИРОВАНЫ** до тех пор, пока в репозитории не будут реализованы и верифицированы все обязательные производственные зависимости рантайма:
> 1. Реальный адаптер `InternetGate` на базе WFP (жизненный цикл `WINDOWS-INTERNET-GATE`);
> 2. Реальный адаптер `PowerController` с получением привилегий (жизненный цикл `WINDOWS-POWER-CONTROLLER`);
> 3. Производственная реализация `IdSource` на базе ОС CSPRNG;
> 4. Производственная реализация `InternetRetryPolicy` (специфицируемая в рамках `WINDOWS-INTERNET-GATE`).

#### Архитектурные гарантии отказа от фиктивных заглушек:
1. **Категорический запрет Dummy-адаптеров**: Внедрение в производственный код бинарника фиктивных заглушек («dummy/stub/noop gate»), возвращающих `Ok(())` или `Err(NotImplemented)`, строжайше запрещено. Служба, докладывающая в SCM статус `SERVICE_RUNNING`, обязана обладать реальной способностью принудительного исполнения.
2. **Честность модели безопасности**: Система родительского контроля, заявляющая операционной системе о своей работоспособности, но не имеющая реального сетевого фильтра, является плацебо и вводит родителя в опасное заблуждение.
3. **Недопустимость подмены семантики Degraded**: Статус `StartupReadiness::Degraded(_)` нормативно предназначен для временных, эксплуатационных сбоев реальных физических адаптеров. Структурное отсутствие адаптера в кодовой базе не является эксплуатационным сбоем — это архитектурный блокер сборки.

---

## 5. Спецификация платформенных зависимостей

### 5.1. Зависимость InternetGate
* `InternetGate` является строго обязательным производственным портом исполнения сетевых ограничений (`SERVICE ENFORCES -> PLATFORM EXECUTES`).
* Отсутствие производственного адаптера `InternetGate` является фатальным архитектурным блокером для реализации `main.rs`.
* Пользовательский интерфейс управления WFP в Windows:
  * Заголовочный файл: `fwpmu.h`
  * Библиотека импорта: `Fwpuclnt.lib`
  * Системная динамическая библиотека: `Fwpuclnt.dll`
* **Владелец реализации**: жизненный цикл **`WINDOWS-INTERNET-GATE DOCUMENTATION`**.
* **Блокирует интеграцию исполняемого файла**: **`ДА`** (`BLOCKS_EXECUTABLE_INTEGRATION: YES`).

### 5.2. Зависимость PowerController
* `PowerController` является обязательным производственным портом воздействия на состояние электропитания рабочей станции.
* **Согласование интерфейса**: Текущий производственный трейт `PowerController` в `crates/service/src/runtime.rs` содержит исключительно метод `initiate_shutdown(&self) -> Result<(), PlatformError>`. Контракт `docs/017` **НЕ ИМЕЕТ ПРАВА** утверждать, что `ServiceRuntime::start()` вызывает несуществующий метод `check_privileges()`.
* Будущий жизненный цикл `WINDOWS-POWER-CONTROLLER` обязан спроектировать и реализовать производственный шов валидации привилегий (`SeShutdownPrivilege`) при конструировании адаптера до передачи его в рантайм, согласованно с фактическим API рантайма.
* Производственный адаптер электропитания должен быть успешно сконструирован и валидирован до того, как служба перейдет в рабочий режим.
* **Владелец реализации**: жизненный цикл **`WINDOWS-POWER-CONTROLLER`**.
* **Блокирует интеграцию исполняемого файла**: **`ДА`** (`BLOCKS_EXECUTABLE_INTEGRATION: YES`).
* **Статус шва привилегий**: `DEFERRED_TO_OWNER_LIFECYCLE_AND_BLOCKING`.

### 5.3. Зависимость IdSource
* `IdSource` используется рантаймом для генерации идентификаторов таймеров (`TimerId`) и записей исходящих сообщений Telegram (`OutboxEntryId`).
* **Владелец реализации**: слой композиции и сервисных утилит крейта `palka-service` (`IDSOURCE_OWNER: palka-service composition/runtime utility layer`).
* **Архитектурные требования к производственной реализации**:
  1. Генерация непрозрачных 128-битных значений (`[u8; 16]`) для `TimerId` и `OutboxEntryId`;
  2. Вероятность коллизии должна быть пренебрежимо мала ($2^{128}$ пространство состояний);
  3. Безопасность и уникальность генерации гарантируются при рестартах процесса без необходимости ведения дисковых счетчиков;
  4. Запрещается использование последовательных или предсказуемых счетчиков в качестве производственной реализации;
  5. Производственная реализация обязана использовать криптографически стойкий генератор случайных чисел, предоставляемый операционной системой (OS-backed CSPRNG), через платформенно-нейтральную абстракцию Rust;
  6. **Запрет прямых вызовов Win32**: Крейт `palka-service` **НЕ ИМЕЕТ ПРАВА** напрямую вызывать `BCryptGenRandom` или иные сырые криптографические API Win32 (`IDSOURCE_DIRECT_WIN32_API: NO`). Это предотвращает циклическую зависимость `service <-> windows-platform` и сохраняет чистую слоистую архитектуру.

### 5.4. Зависимость InternetRetryPolicy
* `InternetRetryPolicy` определяет стратегию повторных попыток согласования сети при получении ошибок от `InternetGate`.
* **Владелец реализации**: жизненный цикл **`WINDOWS-INTERNET-GATE`** (`RETRY_POLICY_OWNER: WINDOWS-INTERNET-GATE`).
* **Блокирует интеграцию исполняемого файла**: **`ДА`** (`BLOCKS_EXECUTABLE_INTEGRATION: YES`).
* **Нормативное разграничение ответственности**:
  * Настоящий контракт `docs/017` **НЕ ОПРЕДЕЛЯЕТ** конкретные числовые константы производственной политики повторов (начальную задержку, множитель роста или максимальную задержку: `RETRY_CONSTANTS_DEFINED_IN_017: NO`). Точные числовые параметры и правила бэкоффа должны быть нормативно зафиксированы будущим контрактом `WINDOWS-INTERNET-GATE`.
  * Сохраняются фундаментальные требования к политике повторов, установленные в `docs/016`:
    - Задержка строго положительна (`delay > 0`);
    - Время задержки ограничено сверху (конечный верхний предел, bounded);
    - Монотонность планирования повторов;
    - Запрет непрерывного («busy») цикла повторов;
    - Немедленная доступность к попытке согласования после рестарта процесса, если в сохраненном состоянии оставалось несогласованное сетевое решение.

---

## 6. Четкое разграничение деградации рантайма и структурного отсутствия адаптеров

Для исключения любой двусмысленности вводится нормативная классификация состояний:

* **СЛУЧАЙ A (Реальный адаптер InternetGate, временный сбой)**:
  В бинарник скомпилирован настоящий производственный WFP-адаптер. При старте системный вызов WFP возвращает временную ошибку (например, `RPC_S_SERVER_UNAVAILABLE` или `ERROR_BUSY`). Рантайм переходит в состояние `StartupReadiness::Degraded(_)`, активирует поток фоновых повторов и разрешает публикацию `SERVICE_RUNNING`, чтобы служба могла автоматически восстановить фильтрацию без падения процесса.
* **СЛУЧАЙ B (Структурное отсутствие адаптера InternetGate)**:
  В репозитории или бинарнике отсутствует настоящий производственный WFP-адаптер. Данное состояние **НЕ ЯВЛЯЕТСЯ** состоянием `Degraded`. Это архитектурный блокер этапа сборки, делающий компиляцию производственного `main.rs` невозможной в рамках Стратегии A (`MISSING_GATE_DISTINGUISHED_FROM_TRANSIENT_FAILURE: YES`).
* **СЛУЧАЙ C (Реальный адаптер PowerController, ошибка исполнения)**:
  В бинарник скомпилирован настоящий производственный адаптер управления питанием. Ошибка при последующей попытке выключения ПК обрабатывается штатными доменными механизмами рантайма как ошибка исполнения действия.
* **СЛУЧАЙ D (Структурное отсутствие адаптера PowerController)**:
  В репозитории отсутствует настоящий производственный адаптер электропитания. Данное состояние **НЕ ЯВЛЯЕТСЯ** эксплуатационным сбоем и блокирует композицию исполняемого файла в рамках Стратегии A (`MISSING_POWER_DISTINGUISHED_FROM_TRANSIENT_FAILURE: YES`).

---

## 7. Трансляция готовности рантайма в статусы SCM (Readiness Mapping)

Интеграционный слой транслирует результаты этапов запуска по следующим строгим правилам:

| Результат этапа | Состояние рантайма | Действие интеграционного слоя | Статус SCM | dwWin32ExitCode |
| :--- | :--- | :--- | :--- | :--- |
| `bootstrap.bootstrap() -> Err(e)` | Рантайм не создавался | Логирование ошибки без секретов, отказ запуска | `SERVICE_STOPPED` | `ERROR_EXCEPTION_IN_SERVICE` (1064) |
| `runtime_factory.start() -> Err(e)` | Фатальный сбой конструктора или восстановления | Логирование ошибки, отказ запуска | `SERVICE_STOPPED` | `ERROR_EXCEPTION_IN_SERVICE` (1064) |
| `Ok(runtime)`, `readiness == Ready(_)` | Все компоненты и адаптеры функционируют штатно | Переход в рабочий режим, доклад RUNNING | `SERVICE_RUNNING` | `NO_ERROR` (0, в процессе работы) |
| `Ok(runtime)`, `readiness == Degraded(_)` | Реальный WFP-адаптер вернул временную ошибку Win32, активен ретрай | Переход в рабочий режим, доклад RUNNING, ретрай в фоне | `SERVICE_RUNNING` | `NO_ERROR` (0, в процессе работы) |

> [!CAUTION]
> Публикация `SERVICE_RUNNING` при `StartupReadiness::Degraded(_)` допускается **ИСКЛЮЧИТЕЛЬНО** в СЛУЧАЕ A (наличие настоящего скомпилированного WFP-адаптера). В СЛУЧАЕ B рантайм не конструируется, и статус `SERVICE_RUNNING` недостижим.

---

## 8. Обработка управляющих сигналов SCM и инварианты остановки (SCM Control Handoff & Stop Invariants)

### 8.1. Реакция на `SERVICE_CONTROL_STOP` и `SERVICE_CONTROL_SHUTDOWN`
1. При поступлении сигнала через `lifecycle.wait_for_control()` интеграционный слой немедленно рапортует:
   ```rust
   lifecycle.report_stop_pending(1, 15_000)?;
   ```
2. Вызывается метод координированной остановки рантайма:
   ```rust
   runtime.stop()?;
   ```
3. `ServiceRuntime::stop()` выставляет атомарный флаг `stop_requested`, прерывает ожидание рабочих потоков (воркеров), дожидается их завершения (`join`) и освобождает системные дескрипторы.

### 8.2. Инварианты сохранности при остановке (Stop Invariants)
* **Запрет автоматического снятия ограничений**: Интеграционный слой службы `palka-service` **НЕ ИМЕЕТ ПРАВА** вызывать `unblock_internet()` только по факту остановки службы (`STOP_CALLS_UNBLOCK: NO`).
* **Разграничение владения жизненным циклом фильтров WFP**:
  ```text
  WFP_FILTER_LIFETIME_DEFINED_IN_017: NO
  WFP_FILTER_LIFETIME_OWNER: WINDOWS-INTERNET-GATE DOCUMENTATION
  ```
  Контракт интеграции `docs/017` **НЕ ОПРЕДЕЛЯЕТ** низкоуровневые механизмы персистентности или динамичности фильтров WFP в ядре Windows (динамические сессии `FWPM_SESSION_FLAG_DYNAMIC`, постоянные фильтры ядра, поведение при аварийном крахе процесса службы или перезагрузке ОС). Эти решения полностью относятся к юрисдикции будущего контракта `WINDOWS-INTERNET-GATE DOCUMENTATION`.
* **Запрет сброса таймеров**: Остановка службы не отменяет активные доменные действия в `state.json`.
* **Запрет очистки Outbox**: Очередь сообщений Telegram сохраняется на диске для обработки при следующем старте.

---

## 9. Политика кодов завершения процесса и фазово-согласованная обработка ошибок (Exit Code & Phase-Aware Policy)

### 9.1. Разграничение внутренней таксономии ошибок и кодов Win32
Будущая реализация интеграции исполняемого файла определяет внутренний типизированный enum Rust, точно отражающий фактические типы возвращаемых ошибок открытых API:
```rust
#[derive(Debug)]
pub enum ServiceIntegrationError {
    Bootstrap(ServiceBootstrapError),
    RuntimeStartup(ServiceRuntimeError),
    RuntimeTeardown(ServiceRuntimeError),
    ScmStatus(ScmRuntimeError),
    DependencyComposition(&'static str),
}
```
* `BOOTSTRAP_ERROR_TYPE: ServiceBootstrapError` (фактический тип из `crates/service/src/bootstrap.rs`).
* `RUNTIME_STOP_PUBLIC_ERROR_TYPE: ServiceRuntimeError` (фактический тип результата публичного метода `ServiceRuntime::stop()`).

Варианты данного Rust-enum **КАТЕГОРИЧЕСКИ НЕ ДОЛЖНЫ** отождествляться с числовыми кодами ошибок Windows SCM.

### 9.2. Семантика системного кода Win32 (`dwWin32ExitCode`)
Поле `dwWin32ExitCode` структуры `SERVICE_STATUS` операционной системы Windows обязано содержать легитимный код ошибки Win32.
Использование произвольных пользовательских чисел `1, 2, 3, 4, 5` в поле `dwWin32ExitCode` строго запрещено стандартами Windows SCM, поскольку эти значения закреплены за стандартными системными ошибками Windows:
* `0` = `NO_ERROR` / `ERROR_SUCCESS`
* `1` = `ERROR_INVALID_FUNCTION`
* `2` = `ERROR_FILE_NOT_FOUND`
* `3` = `ERROR_PATH_NOT_FOUND`
* `4` = `ERROR_TOO_MANY_OPEN_FILES`
* `5` = `ERROR_ACCESS_DENIED`

Для передачи специфичных для службы кодов завершения Windows требует устанавливать:
```text
dwWin32ExitCode = ERROR_SERVICE_SPECIFIC_ERROR (1066)
```
с размещением пользовательского кода в поле `dwServiceSpecificExitCode`.
Текущая реализация адаптера `CanonicalServiceStatus::stopped(win32_exit_code)` в `palka-windows-platform::scm_runtime` всегда устанавливает `dwServiceSpecificExitCode = 0`.
Следовательно, до появления авторизованного расширения SCM-адаптера контракт `docs/017` **ЗАПРЕЩАЕТ** вводить произвольные числовые коды в `dwWin32ExitCode` (`CUSTOM_PALKA_WIN32_CODES_2_3_4_5: NO`).

### 9.3. Фазово-согласованная обработка ошибок (Phase-Aware Error Handling)
Автомат состояний Windows SCM в `ScmServiceContext` категорически **НЕ ДОПУСКАЕТ** прямого нормального перехода:
```text
SERVICE_RUNNING ──► SERVICE_STOPPED
```
(`DIRECT_RUNNING_TO_STOPPED_NORMAL_TRANSITION: NO`). Поэтому обработка ошибок интеграции строго дифференцируется по фазам жизненного цикла (`ERROR_HANDLING_PHASE_AWARE: YES`):

1. **Сбой на этапе START_PENDING**:
   При сбое `bootstrap.bootstrap()`, фабрики `runtime_factory.start(...)` или сбое старта рантайма разрешен штатный терминальный переход:
   `SERVICE_START_PENDING` → `SERVICE_STOPPED` с кодом `ERROR_EXCEPTION_IN_SERVICE` (1064).
2. **Фатальный сбой в состоянии SERVICE_RUNNING**:
   Если служба уже находится в состоянии `SERVICE_RUNNING` и требуется аварийная остановка, интеграционный слой обязан выполнить двухфазную последовательность:
   `SERVICE_RUNNING` → `SERVICE_STOP_PENDING` → `runtime.stop()` (если рантайм существует) → `SERVICE_STOPPED(ERROR_EXCEPTION_IN_SERVICE)`.
   Прямой вызов `report_stopped` в состоянии `RUNNING` отвергается автоматом состояний с ошибкой `InvalidLifecycleTransition`.
3. **Сбой системных вызовов SCM API или нарушение переходов**:
   Если системный вызов `SetServiceStatus` завершился ошибкой или валидный штатный терминальный переход невозможен, оркестрация завершается возвратом ошибки. Внешний аварийный guard диспетчера `ActiveServiceFallback` / Service Entry Return Guard выполняет попытку аварийной публикации статуса. Интеграционный слой не дублирует логику аварийного перехвата.
4. **Сбой диспетчера до вызова `ServiceMain`**:
   До входа в `ServiceMain` дескриптор службы `SERVICE_STATUS_HANDLE` еще не существует. В этом случае вызов `SERVICE_STOPPED` физически невозможен; функция `run_palka_service_dispatcher()` возвращает типизированную ошибку `ScmRuntimeError`.
5. **Отказ остановки рантайма**:
   Ошибка при вызове `runtime.stop()` возвращает `ServiceRuntimeError` (включая внутренний `TeardownError` при сбое джойна рабочих потоков или дисковой персистенции). Контракт **НЕ ИМЕЕТ ПРАВА** утверждать о наличии несуществующего фиксированного тайм-аута остановки.

---

## 10. Защита от паник и аварийный возврат (Panic & Fallback Boundary)

1. **Изоляция FFI-границы**: Ни одна паника Rust не должна пересекать границу вызова `ServiceMain` или обработчика сигналов `HandlerEx`.
2. **Принцип наилучшего старания в ActiveServiceFallback**: Реализация `scm_runtime.rs` содержит аварийный контейнер `ActiveServiceFallback`, перехватывающий паники через `std::panic::catch_unwind`. При нештатном прерывании `ServiceMain` аварийный обработчик предпринимает попытку по принципу наилучшего старания (BEST-EFFORT ATTEMPT) рапортовать в SCM статус `SERVICE_STOPPED` с системным кодом `ERROR_EXCEPTION_IN_SERVICE` (1064).
   * `FALLBACK_SUCCESS_GUARANTEED: NO`
   * `FALLBACK_IS_BEST_EFFORT: YES`
3. **Гарантия публикации терминального статуса**:
   Контракт гарантирует: **СТРОГО НЕ БОЛЕЕ ОДНОЙ УСПЕШНОЙ ПУБЛИКАЦИИ SERVICE_STOPPED** (AT MOST ONE SUCCESSFUL SERVICE_STOPPED PUBLICATION).
   * Штатный путь выполняет терминальную публикацию.
   * Аварийный fallback выполняется исключительно в том случае, если успешная публикация `SERVICE_STOPPED` еще не была зафиксирована.
   * Атомарный флаг `stopped_reported` предотвращает повторные публикации.
   * Если системный вызов `SetServiceStatus` завершается сбоем, сбой остается явным и не подменяется фиктивным успешным рапортом.

---

## 11. Структура файла `main.rs` и фазово-согласованная `PalkaServiceEntry`

### 11.1. Согласование сигнатуры диспетчера
Фактический тип точки входа диспетчера в `palka-windows-platform::scm_runtime`:
```rust
pub type PalkaServiceEntry = fn(ScmServiceContext);
```
Таким образом, функция точки входа службы, передаваемая диспетчеру, возвращает `()`, а не `Result<...>`.

### 11.2. Обертка сервисной точки входа
Сервисная функция-обертка `palka_service_entry(context: ScmServiceContext)` выполняет:
1. Адаптацию платформенного контекста и компонентов к локальным сервисным трейтам:
   * `context` адаптируется к `ServiceLifecyclePort`;
   * `ProductionBootstrap` реализует `ServiceBootstrapPort`;
   * `ProductionRuntimeFactory` реализует `ServiceRuntimeFactory`.
2. Вызов функции тестируемой оркестрации `run_service_with_ports(lifecycle, bootstrap, runtime_factory)`.
3. Внутренняя оркестрация самостоятельно владеет нормальными фазово-согласованными переходами SCM.
4. Если внутренняя оркестрация возвращает ошибку, а терминальный статус не был успешно опубликован (например, из-за сбоя в канале SCM или ошибке `SetServiceStatus`), обертка завершает выполнение и возвращает `()`.
5. В этом случае существующий внешний механизм диспетчера `Service Entry Return Guard` / `ActiveServiceFallback` выполняет аварийную best-effort публикацию `SERVICE_STOPPED` с кодом `ERROR_EXCEPTION_IN_SERVICE` (1064).
6. Обертка **КАТЕГОРИЧЕСКИ НЕ ИМЕЕТ ПРАВА**:
   * Напрямую вызывать функции Win32 API;
   * Обходить автомат состояний `ScmServiceContext`;
   * Дублировать реализацию `ActiveServiceFallback`;
   * Пытаться выполнить недопустимый прямой переход `RUNNING` → `STOPPED`.

### 11.3. Минималистичный Composition Root (`main.rs`)
Файл `crates/service/src/main.rs` проектируется как минималистичный композиционный корень (не более 100 строк кода):
```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    palka_windows_platform::scm_runtime::run_palka_service_dispatcher(palka_service_entry)?;
    Ok(())
}
```
В `main.rs` **ЗАПРЕЩАЕТСЯ** прямой вызов функций Win32 API.

---

## 12. Поведение на платформах, отличных от Windows (Non-Windows Behavior)

* На платформах `!cfg(windows)` функция `run_palka_service_dispatcher` возвращает `Err(ScmRuntimeError::UnsupportedPlatform)`.
* Функция `main()` на не-Windows системах завершает работу с печатью контролируемой ошибки в `stderr` и кодом выхода `1`.
* Это сохраняет возможность компиляции и статической проверки рабочего пространства (`cargo check --workspace --all-targets`) на любых ОС.

---

## 13. Тестируемость и тестовые швы (Testability Seams)

### 13.1. Анализ платформенных ограничений
В текущей кодовой базе:
* `ScmServiceContext::new_test`, `StatusSink` и `MockStatusSink` являются приватными деталями реализации `palka-windows-platform`;
* Вспомогательная функция `bootstrap_service_with_root_fn` является приватной деталью `palka-service::bootstrap`.
Попытка использовать эти приватные конструкторы привела бы к нарушению границ инкапсуляции и расширению платформенного API исключительно ради тестов.

### 13.2. Архитектура четырех изолированных швов тестируемости
Для обеспечения детерминированного тестирования верхнеуровневой логики интеграции без запуска реального диспетчера SCM, без обращений к `%ProgramData%\PALKA` и без реальных адаптеров WFP/Power крейт `palka-service` определяет четыре локальных абстрактных шва:

#### 1. Шов жизненного цикла SCM (`ServiceLifecyclePort`)
```rust
pub trait ServiceLifecyclePort {
    fn report_start_pending(&mut self, checkpoint: u32, wait_hint_ms: u32) -> Result<(), ServiceIntegrationError>;
    fn report_running(&mut self) -> Result<(), ServiceIntegrationError>;
    fn wait_for_control(&self) -> Result<ScmRuntimeControl, ServiceIntegrationError>;
    fn report_stop_pending(&mut self, checkpoint: u32, wait_hint_ms: u32) -> Result<(), ServiceIntegrationError>;
    fn report_stopped(&mut self, win32_exit_code: u32) -> Result<(), ServiceIntegrationError>;
}
```
* **Производственная реализация**: реализуется для `ScmServiceContext`.
* **Тестовая реализация**: локальный тестовый дублер (`FakeServiceLifecyclePort`).

#### 2. Шов начальной загрузки (`ServiceBootstrapPort`)
```rust
pub trait ServiceBootstrapPort {
    fn bootstrap(&mut self) -> Result<BootstrappedServiceState, ServiceBootstrapError>;
}
```
* `BOOTSTRAP_TEST_SEAM: YES`
* **Производственная реализация**: `ProductionBootstrap` однократно вызывает `palka_service::bootstrap::bootstrap_service()`.
* **Тестовая реализация**: `FakeServiceBootstrapPort` детерминированно возвращает `Ok(BootstrappedServiceState)` или `Err(ServiceBootstrapError)` без дискового ввода-вывода.

#### 3. Шов отложенного конструирования рантайма (`ServiceRuntimeFactory`)
```rust
pub trait ServiceRuntimeFactory {
    type Runtime: ServiceRuntimeLifecyclePort;

    fn start(
        &mut self,
        bootstrapped: BootstrappedServiceState,
    ) -> Result<Self::Runtime, ServiceRuntimeError>;
}
```
* `RUNTIME_FACTORY_DEFERRED_UNTIL_AFTER_CP2: YES`
* **Производственная реализация**: `ProductionRuntimeFactory` вызывается строго после завершения Checkpoint 2. Она конструирует реальные производственные зависимости (`InternetGate`, `PowerController`, `SystemClock`, `IdSource`, `InternetRetryPolicy`) и вызывает `ServiceRuntime::start(...)`.
* **Тестовая реализация**: `FakeRuntimeFactory` возвращает тестовый объект без использования WFP и Win32.

#### 4. Шов интерфейса рантайма (`ServiceRuntimeLifecyclePort`)
```rust
pub trait ServiceRuntimeLifecyclePort {
    fn readiness(&self) -> &StartupReadiness;
    fn stop(&mut self) -> Result<(), ServiceRuntimeError>;
}
```
* `RUNTIME_LIFECYCLE_TEST_SEAM: YES`
* **Производственная реализация**: `ServiceRuntime` реализует `ServiceRuntimeLifecyclePort`.
* **Тестовая реализация**: `FakeServiceRuntimeLifecyclePort` детерминированно предоставляет тестовые статусы готовности (`Ready` / `Degraded`) и результат `stop()`.

### 13.3. Каноническая форма тестируемой оркестрации
Оркестрация запуска формулируется через обобщенную тестируемую функцию:
```rust
pub fn run_service_with_ports<L, B, F>(
    mut lifecycle: L,
    mut bootstrap: B,
    mut runtime_factory: F,
) -> Result<(), ServiceIntegrationError>
where
    L: ServiceLifecyclePort,
    B: ServiceBootstrapPort,
    F: ServiceRuntimeFactory,
```

Порядок исполнения внутри `run_service_with_ports` жестко зафиксирован:
1. Публикация CP1: `lifecycle.report_start_pending(1, 30_000)`
2. Начальная загрузка: `bootstrap.bootstrap()`
3. Публикация CP2: `lifecycle.report_start_pending(2, 30_000)`
4. Отложенный запуск рантайма: `runtime_factory.start(bootstrapped)`
5. Проверка готовности: `runtime.readiness()`
6. Публикация RUNNING: `lifecycle.report_running()`
7. Ожидание сигнала: `lifecycle.wait_for_control()`
8. Публикация STOP_PENDING: `lifecycle.report_stop_pending(1, 15_000)`
9. Остановка рантайма: `runtime.stop()`
10. Публикация STOPPED: `lifecycle.report_stopped(NO_ERROR)`

---

## 14. Матрица верификации интеграции (Executable Integration Test Matrix)

Для верификации этапа интеграции исполняемого файла утверждается обязательный набор тестов `EXE-01` .. `EXE-18`:

| Идентификатор | Сценарий верификации | Ожидаемый результат |
| :--- | :--- | :--- |
| **EXE-01** | Делегирование точки входа `main()` диспетчеру SCM | Функция `main()` однократно передает управление `run_palka_service_dispatcher`. |
| **EXE-02** | Публикация Checkpoint 1 до старта Bootstrap | `FakeServiceBootstrapPort` фиксирует вызов строго после публикации `SERVICE_START_PENDING(checkpoint=1)`. |
| **EXE-03** | Отказ Bootstrap блокирует запуск | Сбой `FakeServiceBootstrapPort` с `Err(ServiceBootstrapError)` переводит службу в `SERVICE_STOPPED` с кодом `ERROR_EXCEPTION_IN_SERVICE` (1064), статус `RUNNING` не рапортуется. |
| **EXE-04** | Успех Bootstrap недостаточен для RUNNING | После успешного `FakeServiceBootstrapPort` рапортуется `SERVICE_START_PENDING(checkpoint=2)`, но не `RUNNING`. |
| **EXE-05** | Обязательность завершения Startup Recovery | `FakeRuntimeFactory` вызывается строго после CP2; переход в `SERVICE_RUNNING` происходит строго после успешного завершения `start()` и инспекции `readiness()`. |
| **EXE-06** | Доклад `SERVICE_RUNNING` при `StartupReadiness::Ready(_)` | `FakeServiceRuntimeLifecyclePort` в состоянии Ready переводит службу в `SERVICE_RUNNING` с маской `STOP \| SHUTDOWN`. |
| **EXE-07** | Обработка `StartupReadiness::Degraded(_)` | При временной ошибке WFP рапортуется `SERVICE_RUNNING` и активируется фоновый ретрай. |
| **EXE-08** | Фатальный сбой старта рантайма | Сбой `FakeRuntimeFactory::start(...) -> Err(ServiceRuntimeError)` рапортует `SERVICE_STOPPED` с кодом `ERROR_EXCEPTION_IN_SERVICE` (1064). |
| **EXE-09** | Обработка управляющего сигнала STOP | Прием `SERVICE_CONTROL_STOP` вызывает `STOP_PENDING` -> `runtime.stop()` -> `SERVICE_STOPPED(0)`. |
| **EXE-10** | Обработка управляющего сигнала SHUTDOWN | Прием `SERVICE_CONTROL_SHUTDOWN` выполняет плановый teardown и рапортует `SERVICE_STOPPED(0)`. |
| **EXE-11** | Ошибка остановки рантайма | Сбой `FakeServiceRuntimeLifecyclePort::stop() -> Err(ServiceRuntimeError)` приводит к завершению службы с кодом `ERROR_EXCEPTION_IN_SERVICE` (1064). |
| **EXE-12** | Запрет разблокировки сети при остановке | При завершении службы сетевой порт шлюза не получает команду `unblock_internet`. |
| **EXE-13** | Сохранность персистентного состояния при остановке | Таймеры, действия и очередь Telegram Outbox сохраняются в `state.json` при остановке. |
| **EXE-14** | Не более одной успешной публикации `SERVICE_STOPPED` | Гарантируется строго не более одной успешной публикации `SERVICE_STOPPED` (at most one successful publication). |
| **EXE-15** | Перехват паники в `ServiceMain` | Паника в теле сервисной функции перехватывается fallback-обработчиком с best-effort попыткой рапорта `SERVICE_STOPPED` с кодом `ERROR_EXCEPTION_IN_SERVICE` (1064). |
| **EXE-16** | Отсутствие доменных переходов в интеграционном слое | Интеграционный слой не содержит вызовов переходов состояний или модификаций таймеров. |
| **EXE-17** | Отсутствие прямых низкоуровневых платформенных вызовов | В крейте `palka-service` отсутствуют прямые вызовы WFP/Power/Win32 API в обход абстракций. |
| **EXE-18** | Защита производственного шлюза зависимостей | Попытка сборки релизной службы с фиктивными/заглушечными адаптерами пресекается архитектурным барьером Стратегии A. |

---

## 15. Решение по следующему жизненному циклу (Next Implementation Dependency Decision)

На основании нормативного выбора **СТРАТЕГИИ A (DEPENDENCY-FIRST)** в Разделе 4 устанавливается следующее каноническое решение:

```text
NEXT_LIFECYCLE:
WINDOWS-INTERNET-GATE DOCUMENTATION
```

### Обоснование:
1. Реализация исполняемого файла `crates/service/src/main.rs` не может быть осуществлена без реального производственного адаптера `InternetGate` и производственной политики `InternetRetryPolicy`.
2. Подмена `InternetGate` тестовой заглушкой в производственном бинарнике категорически запрещена правилами безопасности и продуктовой честности PALKA.
3. Сетевой шлюз `InternetGate` является первичной платформенной зависимостью рантайма, опрашиваемой и согласуемой немедленно на этапе начального восстановления (Startup Recovery, Фаза I).
4. Следовательно, непосредственным следующим жизненным циклом проекта является разработка нормативного контракта реального сетевого шлюза Windows Filtering Platform: **`WINDOWS-INTERNET-GATE DOCUMENTATION`**.
5. После разработки и закрытия контрактов и реализаций `WINDOWS-INTERNET-GATE` и `WINDOWS-POWER-CONTROLLER` жизненный цикл `SERVICE-SCM-EXECUTABLE-INTEGRATION IMPLEMENTATION` будет полностью разблокирован для финализации `main.rs`.
