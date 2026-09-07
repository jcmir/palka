# Контракт производственного управления питанием Windows (Windows PowerController Contract)

Документ определяет нормативный контракт производственной реализации подсистемы управления питанием (`PowerController`) для операционной системы Windows в проекте PALKA (V1). Контракт устанавливает правила взаимодействия со службой `palka-service`, точные Win32 API, семантику системных привилегий, таксономию ошибок, архитектуру тестирования и границы доказательств.

---

## 1. Цель и область действия (Purpose and Scope)

### 1.1. Назначение
Компонент `WindowsPowerController` отвечает за выполнение системного выключения рабочей станции Windows при наступлении дедлайна родительского контроля (`ActionKind::ShutdownComputer`). Компонент транслирует абстрактную команду рантайма `PowerController::initiate_shutdown()` в низкоуровневый системный вызов Windows API, соблюдая требования безопасности контекста службы, предотвращения задержки вызова диалогами сохранения приложений пользователя (через флаг `bForceAppsClosed=TRUE`), устранения окна тайм-аута ОС (через `dwTimeout=0`) и детерминированной обработки платформенных отказов.

При этом контракт строго фиксирует, что успешный прием команды выключения операционной системой (`REQUEST_ACCEPTED`) является асинхронным и не предоставляет гарантии немедленного физического обесточивания аппаратной платформы или универсальной необратимости системного процесса. Компонент `PowerController` не предоставляет API отмены после наступления дедлайна.

### 1.2. Входит в область действия (In-Scope)
- Спецификация канонического Win32 API выключения системы и его параметров;
- Контекст безопасности службы (`LocalSystem`, Session 0);
- Модель проверки наличия привилегии `SeShutdownPrivilege` при создании адаптера (Readiness Probe);
- Безопасная двухэтапная семантика запроса информации о токене через `GetTokenInformation`;
- Семантика включения и восстановления привилегий (`AdjustTokenPrivileges`);
- Таксономия платформенных ошибок `WindowsPowerError` и правила их маппинга в `PlatformError` службы;
- Структура сервисного адаптера (`crates/service/src/windows_power_controller.rs`);
- Внутренний тестовый шов `WindowsPowerPort` для модульного тестирования без выключения машины разработчика;
- Поведение на неподдерживаемых платформах (не-Windows);
- Нормативная матрица верификации (PWR-01 .. PWR-38);
- Граница доказательств для физического исполнения в Windows.

### 1.3. Выходит за рамки области действия (Out-of-Scope)
- Расписание и доменные правила планирования таймеров (регулируются `docs/003`, `docs/016`);
- Расчет и отправка предупреждающих уведомлений (60m, 30m, 20m, 10m, 3m) — зона ответственности `runtime` и `tray`;
- Протокол Telegram и IPC-транспорт (`docs/007`, `docs/008`);
- Сетевая фильтрация WFP (`docs/018`);
- Генерация идентификаторов `IdSource` (отдельный жизненный цикл);
- Интеграция исполняемого файла службы SCM (`docs/017`).

---

## 2. Согласование с существующими контрактами

### 2.1. Отношение к docs/006-power-contract.md
Документ `docs/006` фиксирует ранний концептуальный уровень управления питанием. Настоящий контракт `docs/019` является нормативным и приоритетным для производственной реализации подсистемы Windows:
1. **Трейт рантайма**: Трейт в `docs/006` концептуально содержал метод `check_privileges()`. В действующей архитектуре generic-трейт рантайма в `crates/service/src/runtime.rs` строго зафиксирован:
   ```rust
   pub trait PowerController: Send + Sync {
       fn initiate_shutdown(&self) -> Result<(), PlatformError>;
   }
   ```
   Контракт `docs/019` **НЕ ДОБАВЛЯЕТ** метод `check_privileges()` в трейт `PowerController`.
2. **Проверка готовности**: Проверка наличия привилегий инкапсулируется в конструкторе производственного адаптера `WindowsPowerController` и вызывается на этапе компоновки службы (`ProductionRuntimeFactory`).
3. **Уведомления и тайминги**: Описанные в `docs/006` отправка сообщений в Telegram и показ предупреждений относятся к уровню оркестрации рантайма и не дублируются в низкоуровневом адаптере питания.

### 2.2. Отношение к docs/016-service-runtime-orchestration-contract.md
Контракт соблюдает архитектурный инвариант:
> **CORE DECIDES, SERVICE ENFORCES, PLATFORM EXECUTES, RUNTIME SERIALIZES AUTHORITATIVE MUTATION**

- Рантайм службы гарантирует, что состояние `Executing` персистируется на диск в `state.json` **ДО** входа в платформенный эффект `PowerController::initiate_shutdown()`.
- При успехе рантайм переводит состояние действия в `Completed` и фиксирует статус `ShutdownState::InProgress`.
- При ошибке рантайм переводит действие в `Failed { reason }`, переводит здоровье в `HealthStatus::Degraded` и ставит аварийное уведомление в `telegram_outbox`.
- При процедуре восстановления службы после рестарта (Startup Recovery, RT-03) просроченное действие выключения (`remaining <= 0`) переводится в `Missed` и **НИКОГДА НЕ ВЫЗЫВАЕТ** `PowerController::initiate_shutdown()`.
- Платформенный адаптер не принимает доменных решений и не выполняет повторных попыток выключения при получении ошибки от ОС.

### 2.3. Отношение к docs/017-service-scm-executable-integration-contract.md
1. **Нормативный источник стратегии (Strategy Source)**:
   Архитектурное правило Strategy A (dependency-first интеграция служб перед сборкой бинарного файла SCM), сформулированное в `docs/017` (раздел 5.2), остается строго нормативным.
2. **Текущий статус репозитория и управления (Current Repository / Governance Status)**:
   В тексте `docs/017` зафиксирован исторический снимок готовности компонентов на момент составления того документа. Фактический статус зависимостей в репозитории на текущий момент таков:
   - `InternetGate` = **READY** (реализован в рамках жизненного цикла `WINDOWS-INTERNET-GATE`, PR #31, коммит `6084b5c267915704d5f1d0223ae583c1134be9ac`);
   - `InternetRetryPolicy` = **READY** (входит в сервисный адаптер `crates/service/src/windows_internet_gate.rs`);
   - `RuntimeClock` / `SystemClock` = **READY**;
   - `Bootstrap` = **READY**;
   - `StateStore` = **READY**;
   - `PowerController` = **MISSING** (находится в статусе отсутствующего до закрытия жизненного цикла реализации настоящего контракта);
   - `IdSource` = **MISSING** (производственный CSPRNG генератор идентификаторов).
3. **Статус разблокировки после PowerController**:
   После успешного закрытия жизненного цикла реализации `PowerController` зависимость `PRODUCTION_ID_SOURCE` остается единственным блокирующим фактором для финальной интеграции исполняемого файла службы по Strategy A. Документ `docs/017` в рамках текущей операции не модифицируется.

---

## 3. Выбор Win32 API и параметры запроса

### 3.1. Канонический системный API
Производственная реализация для Windows ОБЯЗАНА использовать API:
```c
InitiateSystemShutdownExW
```
(библиотека `Advapi32.dll`, модуль windows-rs `windows::Win32::System::Shutdown`).

#### Обоснование отклонения альтернатив:
- **`ExitWindowsEx`**: **КАТЕГОРИЧЕСКИ ЗАПРЕЩЕН**. Документация Microsoft Learn устанавливает, что API `ExitWindowsEx` спроектирован для завершения процессов в сеансе входа вызывающего потока (caller's logon session). Когда вызывающий процесс не является интерактивным пользователем (в частности, для системной службы Windows, функционирующей в Session 0), вызов `ExitWindowsEx` может вернуть успешный статус завершения, фактически не производя выключения компьютера. Вследствие этого служба Windows не может использовать `ExitWindowsEx` в качестве производственного примитива выключения рабочей станции PALKA. PALKA V1 использует исключительно `InitiateSystemShutdownExW`.
- **`InitiateShutdownW`**: Является допустимым альтернативным API, однако `InitiateSystemShutdownExW` выбран как канонический V1, поскольку его сигнатура обеспечивает строгое взаимно однозначное соответствие булевых флагов требованиям PALKA без необходимости комбинирования битовых масок.
- **Внешние утилиты (`shutdown.exe`, `cmd.exe`, `powershell.exe`, `Stop-Computer`)**: **КАТЕГОРИЧЕСКИ ЗАПРЕЩЕНЫ**. Вызов внешних процессов нарушает детерминизм, создает векторы подмены бинарных файлов и задерживает обработку ошибок.

### 3.2. Точные параметры системного вызова
Вызов `InitiateSystemShutdownExW` обязан производиться со следующими неизменными параметрами:

```rust
InitiateSystemShutdownExW(
    PCWSTR::null(),        // lpMachineName: NULL (локальный компьютер)
    PCWSTR::null(),        // lpMessage: NULL (без всплывающего сообщения)
    0,                     // dwTimeout: 0 (немедленное завершение)
    true,                  // bForceAppsClosed: TRUE (принудительное закрытие приложений)
    false,                 // bRebootAfterShutdown: FALSE (выключение, а не перезагрузка)
    SHUTDOWN_REASON(0x80000000), // dwReason: SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_MINOR_OTHER | SHTDN_REASON_FLAG_PLANNED
)
```

Реальная проекция `windows` 0.62.2 объявляет системную функцию в обобщенном виде:
```rust
pub unsafe fn InitiateSystemShutdownExW<P0, P1>(
    lpmachinename: P0,
    lpmessage: P1,
    dwtimeout: u32,
    bforceappsclosed: bool,
    brebootaftershutdown: bool,
    dwreason: SHUTDOWN_REASON,
) -> windows_core::Result<()>
where
    P0: windows_core::Param<windows_core::PCWSTR>,
    P1: windows_core::Param<windows_core::PCWSTR>,
```
где передача `PCWSTR::null()` в качестве `lpmachinename` и `lpmessage` полностью поддерживается реализацией типажа `windows_core::Param<PCWSTR>`.

1. **`lpMachineName` = NULL**: Запрос исполняется строго локально. Удаленное управление выключением в V1 не поддерживается.
2. **`lpMessage` = NULL**: При `dwTimeout = 0` диалоговое окно завершения работы операционной системой не отрисовывается. Предупреждения пользователю заблаговременно выводятся через PALKA Tray.
3. **`dwTimeout` = 0**: Система инициирует завершение работы немедленно в момент дедлайна. Никаких дополнительных искусственных обратных отсчетов операционной системы не вводится.
4. **`bForceAppsClosed` = TRUE**: Операционная система принудительно завершает приложения с несохраненными изменениями, предотвращая появление блокирующих диалогов ("Сохранить файл перед выходом?"), с помощью которых непривилегированный пользователь мог бы задерживать выключение. Возможность потери несохраненных данных пользователя является осознанным компромиссом строгого родительского контроля.
5. **`bRebootAfterShutdown` = FALSE**: Запрашивается завершение работы системы без последующего перезапуска (shutdown without a subsequent restart). Значение `FALSE` указывает операционной системе на необходимость останова, а не перезагрузки. При этом возврат `Ok(())` (`REQUEST_ACCEPTED`) не является доказательством физического обесточивания аппаратной платформы в момент вызова; фактическое выключение питания классифицировано как требование интеграционного тестирования в реальной среде Windows (`WINDOWS_INTEGRATION_REQUIRED`, включая сценарий PWR-34).
6. **`dwReason` = 0x80000000**:
   - `SHTDN_REASON_MAJOR_OTHER` (0x00000000)
   - `SHTDN_REASON_MINOR_OTHER` (0x00000000)
   - `SHTDN_REASON_FLAG_PLANNED` (0x80000000)
   - Человекочитаемое описание в журнале событий: **Other (Planned)**. Запланированное выключение родительского контроля не является сбоем оборудования, операционной системы или прикладного ПО. Флаг `SHTDN_REASON_FLAG_USER_DEFINED` (0x40000000) не используется, так как V1 не регистрирует пользовательские коды причин в реестре Windows.

### 3.3. Семантика результата вызова: REQUEST_ACCEPTED
Возврат `Ok(())` от метода `PowerController::initiate_shutdown()` имеет строго следующую нормативную семантику:
- **`Ok(())` означает `REQUEST_ACCEPTED`**: Операционная система проверила параметры, валидировала привилегии процесса и успешно приняла команду выключения в обработку.
- **`SHUTDOWN_COMPLETION_IS_ASYNCHRONOUS: YES`**: Процесс завершения работы операционной системы является асинхронным.
- **`PHYSICAL_POWEROFF_PROVEN_BY_RETURN: NO`**: Возврат `Ok(())` **НЕ ДОКАЗЫВАЕТ**, что системная плата физически обесточена в момент возврата управления. Фактическое отключение питания происходит позже и зависит от выгрузки драйверов и системных служб ОС.
- **Отсутствие гарантии безусловного завершения**: Системный вызов не дает математической гарантии, что аппаратный сбой или зависший системный драйвер уровня ядра не помешают обесточиванию. Проверка физического выключения относится к интеграционному тестированию.

### 3.4. Граница отмены (Abort Boundary)
- При `dwTimeout = 0` окно обратного отсчета Windows отсутствует, вследствие чего вызов API `AbortSystemShutdownW` технически не может отменить процедуру через стандартный тайм-аут ОС.
- Компонент `PowerController` **НЕ ПРЕДОСТАВЛЯЕТ** метода `abort_shutdown()` (`POWER_CONTROLLER_ABORT_API: NO`).
- Доменная отмена выключения в PALKA допустима исключительно **ДО** наступления дедлайна (`remaining > 0`), когда таймер отменяется на уровне рантайма службы и платформенный вызов вообще не совершается. После наступления дедлайна отмена категорически запрещена.

---

## 4. Контекст безопасности и модель привилегий

### 4.1. Контекст безопасности процесса
- Производственный компонент `WindowsPowerController` исполняется в контексте доверенного процесса службы Windows (`NT AUTHORITY\SYSTEM`, `LocalSystem`), работающего в изолированной Session 0.
- Системные вызовы управления токеном и выключения ОБЯЗАНЫ использовать контекст безопасности процесса (`OpenProcessToken(GetCurrentProcess(), ...)`).
- Платформенный адаптер **КАТЕГОРИЧЕСКИ НЕ ДОЛЖЕН** использовать токен имперсонации клиента IPC, токен пользователя сеанса или учетные данные внешних каналов. Любая имперсонация потока перед вызовом управления питанием должна быть снята.

### 4.2. Привилегия SeShutdownPrivilege
В соответствии с архитектурой безопасности Windows:
1. Учетная запись `LocalSystem` по умолчанию наделена привилегией `SeShutdownPrivilege` (`SE_SHUTDOWN_NAME`).
2. В токене процесса привилегия по умолчанию создается в **отключенном** состоянии (`Attributes = 0`), а не `SE_PRIVILEGE_ENABLED`.
3. Вызов `InitiateSystemShutdownExW` без предварительного включения привилегии завершается ошибкой `ERROR_ACCESS_DENIED` (код 5).
4. Функция `AdjustTokenPrivileges` способна только включать или отключать уже назначенные токену привилегии, но **не может** добавлять отсутствующие.

### 4.3. Проверка готовности в конструкторе (Readiness Probe)
Конструирование `WindowsPowerController` обязано выполнять проверку готовности токена в строго безотказном и безопасном режиме (Read-Only Probe):
1. Процесс открывает собственный токен с правом чтения: `OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_handle)`.
2. Разрешается LUID для имени `SE_SHUTDOWN_NAME` через `LookupPrivilegeValueW`.
3. Запрашивается список назначенных привилегий через `GetTokenInformation` (см. протокол в разделе 4.4).
4. Выполняется поиск разрешенного LUID среди назначенных привилегий токена.
5. Дескриптор токена освобождается через RAII `SafeHandle`.

**Нормативное требование**: Конструктор **КАТЕГОРИЧЕСКИ НЕ ДОЛЖЕН** включать привилегию `SeShutdownPrivilege` в постоянном режиме. Принцип наименьших привилегий (Least Privilege) требует, чтобы процесс службы не удерживал привилегию выключения включенной во время штатной многодневной работы.

### 4.4. Двухэтапная безопасная семантика GetTokenInformation
Запрос списка привилегий токена через `GetTokenInformation(TokenPrivileges)` обязан выполняться по строгому двухэтапному протоколу с гарантией безопасности памяти (Memory Safety & Fail-Closed):
1. **Первый вызов (определение размера нативного буфера)**:
   - Вызывается `GetTokenInformation` с `token_information = NULL` и `token_information_length = 0`.
   - Возврат кода ошибки `ERROR_INSUFFICIENT_BUFFER` от `GetLastError()` является **ОЖИДАЕМЫМ ШТАТНЫМ РЕЗУЛЬТАТОМ**, а не сбоем запроса.
   - Захватывается значение `return_length` — количество байт, запрошенное операционной системой Windows для нативного результата переменной длины.
2. **Разделение требований к аллокации и валидации нативной полезной нагрузки**:
   - **Требование к аллокации и объектной безопасности (Allocation / Object-Safety Requirement)**: Реализация обязана выделить достаточно объемное, гарантированно выровненное и инициализированное базовое хранилище (backing storage) для выбранной безопасной стратегии доступа. Если реализация формирует типизированный объект или ссылку на проекцию `TOKEN_PRIVILEGES`, ее базовое выделение памяти обязано быть достаточным для этого представления в памяти Rust. Контракт не закрепощает конкретный тип аллокации.
   - **Валидация нативной полезной нагрузки (Native Payload Validation)**: `return_length` фиксирует число байт, запрошенных Windows для размещения структуры переменной длины. Валидация обязана гарантировать наличие достаточного количества байт для безопасного извлечения заголовка и значения `PrivilegeCount` в соответствии с выбранной стратегией доступа. Поскольку в проекции `windows` 0.62.2 структура `TOKEN_PRIVILEGES` содержит одноэлементный плейсхолдер `Privileges: [LUID_AND_ATTRIBUTES; 1]` (`ANYSIZE_ARRAY`), размер `std::mem::size_of::<TOKEN_PRIVILEGES>()` включает память под эту плейсхолдер-запись и **НЕ ДОЛЖЕН** автоматически считаться универсальным семантическим минимумом для всякой полезной нагрузки переменной длины до того, как значение `PrivilegeCount` стало известно. Случай `PrivilegeCount == 0` не должен становиться недостижимым исключительно из-за наличия одноэлементного плейсхолдера в определении Rust-структуры.
3. **Выравнивание буфера (Alignment Safety)**:
   - Память для приема структур `TOKEN_PRIVILEGES` и `LUID_AND_ATTRIBUTES` обязана быть строго выровнена в соответствии с требованиями структуры (не менее `std::mem::align_of::<TOKEN_PRIVILEGES>()`). Контракт категорически запрещает небезопасное приведение сырого байтового вектора `Vec<u8>` произвольного выравнивания (unsafe casting). Выравнивание обязано гарантироваться типом буфера до передачи указателя в Win32 API.
4. **Второй вызов (получение данных)**:
   - Вызывается `GetTokenInformation` с выделенным выровненным буфером. В случае возврата ошибки вызов прерывается с `TokenPrivilegeQueryFailure { win32_code }`.
5. **Безопасная обработка PrivilegeCount и проверенная арифметика границ (Bounds & Overflow Safety)**:
   - Поле `PrivilegeCount` считывается как `u32` и ОБЯЗАНО валидироваться **ДО** любого вычитания, индексации элементов, итерации или вычисления адресов памяти.
   - **Случай `PrivilegeCount == 0`**:
     - Представляет собой структурно валидный результат, означающий отсутствие назначенных привилегий в токене;
     - Исключает обращение к плейсхолдеру первого элемента `Privileges[0]`;
     - Исключает целочисленное переполнение при вычитании (subtraction underflow);
     - Исключает итерацию по гибкому массиву (flexible-array iteration) и любую адресную арифметику за пределами валидного заголовка;
     - Для процедуры проверки готовности в конструкторе (Readiness Probe) возвращает ошибку `WindowsPowerError::PrivilegeNotAssigned`.
   - **Случай `PrivilegeCount > 0`**:
     - Значение счетчика нормализуется / преобразуется в целочисленный тип, используемый для вычисления размера буфера перед операциями умножения или сложения (все операнды единой цепочки вычислений обязаны использовать совместимые типы);
     - Преобразование обязано быть точным и безопасным (checked / lossless) для всех поддерживаемых целевых архитектур Windows;
     - Требуемый размер динамической полезной нагрузки для переменного числа элементов вычисляется исключительно с использованием проверенной арифметики (checked arithmetic);
     - Компоновка структуры в `windows` 0.62.2 с одним встроенным плейсхолдером `LUID_AND_ATTRIBUTES` обязана учитываться ровно один раз (двойной учет первого элемента запрещен);
     - Проверенное сложение, умножение и вычитание (или эквивалентно безопасная формулировка) обязаны при любом переполнении приводить к немедленному отказу (fail-closed);
     - Рассчитанный требуемый размер полезной нагрузки обязан полностью укладываться в валидный диапазон возвращенного буфера (`return_length`) до разыменования или индексации любого элемента массива.
   - **Принцип Fail-Closed**: Любое арифметическое переполнение при вычислениях, усеченный или несогласованный размер полезной нагрузки (рассчитанный размер превышает фактически возвращенный `return_length`), поврежденная структура или выход за границы обязаны немедленно приводить к отказу с `TokenPrivilegeQueryFailure` до разыменования или индексации нативной памяти процесса.

### 4.5. Включение привилегии при вызове (initiate_shutdown)
Непосредственно перед вызовом `InitiateSystemShutdownExW`:
1. Токен процесса открывается с правами: `TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY`.
2. Формируется структура `TOKEN_PRIVILEGES` с атрибутом `SE_PRIVILEGE_ENABLED` для LUID `SeShutdownPrivilege`.
3. Вызывается `AdjustTokenPrivileges` с передачей буфера для сохранения `PreviousState`.
4. **Контроль GetLastError**:
   - Возврат ненулевого значения от `AdjustTokenPrivileges` не гарантирует включения привилегии. Код ошибки проверяется немедленно.
   - Если `GetLastError() == ERROR_NOT_ALL_ASSIGNED` (1300), адаптер возвращает ошибку `WindowsPowerError::PrivilegeNotAssigned`.
   - Если функция вернула 0, возвращается `WindowsPowerError::AdjustPrivilegeFailure { win32_code }`.
   - Только при `GetLastError() == ERROR_SUCCESS` (0) привилегия считается успешно активированной.

### 4.6. Политика восстановления привилегий (Privilege Restoration Policy)
Политика восстановления предыдущего состояния привилегии строго регламентирует исход операции:

```mermaid
flowchart TD
    A[Вызов InitiateSystemShutdownExW] --> B{Результат вызова?}
    B -->|Успех Ok| C[REQUEST_ACCEPTED: Обязательное восстановление НЕ выполняется. Возврат Ok(())]
    B -->|Ошибка Err| D[Явный вызов AdjustTokenPrivileges для PreviousState]
    D --> E{Восстановление успешно?}
    E -->|Да| F[Возврат исходной ошибки выключения ShutdownRequestFailure / ShutdownAlreadyInProgress]
    E -->|Нет| G[Возврат PrivilegeRestoreFailure win32_code]
```

1. **Случай А: Вызов выключения завершился ошибкой (Err)**:
   - Адаптер ОБЯЗАН немедленно вызвать `AdjustTokenPrivileges` для возврата токена в состояние `PreviousState`, полученное при первом вызове. В соответствии с нормативной семантикой Win32, захваченная структура `PreviousState` передается без искажения в качестве `NewState`.
   - Если восстановление успешно: возвращается исходная типизированная ошибка системного вызова (`ShutdownRequestFailure` или `ShutdownAlreadyInProgress`).
   - Если восстановление завершилось сбоем: возвращается `WindowsPowerError::PrivilegeRestoreFailure { win32_code }`. Ошибка восстановления имеет наивысший приоритет возврата, поскольку процесс службы остался в неопределенном с точки зрения безопасности состоянии.
2. **Случай Б: Вызов выключения успешен (Ok)**:
   - Адаптер возвращает `Ok(())` (`REQUEST_ACCEPTED`).
   - Обязательное восстановление привилегий в V1 **НЕ ВЫПОЛНЯЕТСЯ** (`PRIVILEGE_RESTORE_AFTER_REQUEST_ACCEPTED: NO`).
   - **Обоснование и компромисс безопасности (Accepted Security Trade-off)**: Трейт рантайма ожидает `Result<(), PlatformError>`. После того как операционная система приняла запрос на выключение, служба не имеет права ложно объявлять операцию сбойной из-за возможной ошибки очистки привилегии. Если в исключительной ситуации физическое выключение ОС будет сорвано сторонними факторами, привилегия `SeShutdownPrivilege` может остаться включенной в доверенном процессе службы `LocalSystem` до его завершения. Это является утвержденным проектным компромиссом V1.

---

## 5. Таксономия ошибок и маппинг в службу

### 5.1. Типизированные ошибки платформы: WindowsPowerError
Подсистема `windows-platform` определяет строго фиксированный enum ошибок:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowsPowerError {
    /// Вызов выполнен на неподдерживаемой платформе (не-Windows)
    UnsupportedPlatform,

    /// Сбой системного вызова OpenProcessToken
    OpenProcessTokenFailure { win32_code: u32 },

    /// Сбой системного вызова LookupPrivilegeValueW
    LookupPrivilegeFailure { win32_code: u32 },

    /// Сбой запроса или проверки буфера GetTokenInformation
    TokenPrivilegeQueryFailure { win32_code: u32 },

    /// Привилегия SeShutdownPrivilege отсутствует в токене процесса (ERROR_NOT_ALL_ASSIGNED)
    PrivilegeNotAssigned,

    /// Сбой системного вызова AdjustTokenPrivileges при включении привилегии
    AdjustPrivilegeFailure { win32_code: u32 },

    /// Сбой системного вызова AdjustTokenPrivileges при откате привилегии после ошибки выключения
    PrivilegeRestoreFailure { win32_code: u32 },

    /// Сбой системного вызова InitiateSystemShutdownExW
    ShutdownRequestFailure { win32_code: u32 },

    /// Процедура завершения работы уже инициирована в операционной системе (ERROR_SHUTDOWN_IN_PROGRESS = 1115)
    ShutdownAlreadyInProgress { win32_code: u32 },
}
```

### 5.2. Обработка ERROR_SHUTDOWN_IN_PROGRESS (код 1115)
- Если `InitiateSystemShutdownExW` возвращает код `ERROR_SHUTDOWN_IN_PROGRESS` (1115 / `0x0000045B`), ошибка транслируется строго в `WindowsPowerError::ShutdownAlreadyInProgress { win32_code: 1115 }`.
- **Запрет поглощения**: Данный статус **КАТЕГОРИЧЕСКИ НЕЛЬЗЯ** трактовать как успех `Ok(())`. Проект PALKA не может доказать, что уже идущее выключение было запрошено с флагом `bRebootAfterShutdown = FALSE` (это может быть перезагрузка ОС после установки стороннего обновления). Попытка должна фиксироваться как сбой конкретного доменного действия службы.

### 5.3. Сервисный адаптер: crates/service/src/windows_power_controller.rs
Связывание платформенного адаптера со службой реализуется на стороне `crates/service` (направление зависимости `palka-service -> palka-windows-platform`):
```rust
impl PowerController for WindowsPowerControllerAdapter {
    fn initiate_shutdown(&self) -> Result<(), PlatformError> {
        self.inner.initiate_shutdown().map_err(|err| PlatformError {
            reason: format!("Windows power failure: {err:?}"),
        })
    }
}
```

Правила формирования диагностической строки `PlatformError.reason`:
1. **Варианты с числовым кодом ошибки**: Варианты `WindowsPowerError`, содержащие поле `win32_code` (`OpenProcessTokenFailure`, `LookupPrivilegeFailure`, `TokenPrivilegeQueryFailure`, `AdjustPrivilegeFailure`, `PrivilegeRestoreFailure`, `ShutdownRequestFailure`, `ShutdownAlreadyInProgress`), ОБЯЗАНЫ сохранять этот числовой код Win32 в строке `reason` для возможности точной диагностики через журнал событий и телеметрию службы.
2. **Семантические варианты без числового кода**: Варианты, не содержащие поля `win32_code` (`UnsupportedPlatform` и `PrivilegeNotAssigned`), представляются в строке `reason` своим точным именем семантического варианта.
3. **Неизменность таксономии**: Точная 9-вариантная таксономия `WindowsPowerError` остается строго неизменной. Запрещается добавлять поле `win32_code` в вариант `PrivilegeNotAssigned` или создавать десятый вариант ошибки.

---

## 6. Архитектура декомпозиции и тестовый шов

### 6.1. Декомпозиция файлов в windows-platform
Реализация управления питанием разделяется на два модуля:
1. `crates/windows-platform/src/power.rs`:
   - Публичный тип `WindowsPowerController`;
   - Таксономия ошибок `WindowsPowerError`;
   - Внутренний тестовый шов `WindowsPowerPort`;
   - Константы выключения и логика оркестрации;
   - Тестовые дублеры `FakeWindowsPowerPort` и модульные тесты.
2. `crates/windows-platform/src/power_windows.rs` (активен при `#[cfg(windows)]`):
   - Реализация `WindowsPowerPort` поверх настоящих Win32 API;
   - Безопасные обертки `SafeHandle` с RAII вызовом `CloseHandle`;
   - Реализация двухэтапного запроса `GetTokenInformation`;
   - Вызовы `AdjustTokenPrivileges` и `InitiateSystemShutdownExW`.

### 6.2. Внутренний тестовый шов: WindowsPowerPort
Для исключения случайного выключения рабочего компьютера разработчика при запуске `cargo test`, платформенный адаптер параметризуется внутренним тестовым швом `WindowsPowerPort` (с реализацией по умолчанию для Windows и дублером `FakeWindowsPowerPort` для модульных тестов).

Контракт намеренно **НЕ ФИКСИРУЕТ** жесткую псевдо-Rust сигнатуру трейта `WindowsPowerPort` на уровне контракта документации, чтобы избежать преждевременного закрепощения низкоуровневых структурных деталей, искажающих нативную семантику Win32 или препятствующих полноценному доказательству матрицы верификации. Вместо этого контракт формулирует **нормативные поведенческие требования** к внутреннему тестовому шву:

1. **Наблюдаемость всех аргументов вызова выключения (PWR-01)**:
   Тестовый шов обязан фиксировать и делать доступными для проверки в Unit-тестах все аргументы вызова `InitiateSystemShutdownExW`, включая:
   - `lpMachineName == NULL` (локальный компьютер);
   - `lpMessage == NULL` (отсутствие текста сообщения);
   - `dwTimeout == 0` (нулевой таймаут);
   - `bForceAppsClosed == TRUE` (принудительное закрытие приложений);
   - `bRebootAfterShutdown == FALSE` (выключение, а не перезагрузка);
   - `dwReason == 0x80000000` (код причины `SHTDN_REASON_FLAG_PLANNED`).
2. **Детерминированное доказательство обеих фаз GetTokenInformation (PWR-04, PWR-05)**:
   Тестовый шов обязан позволять изолированную и независимую проверку двух вызовов протокола запроса размера и содержимого токена:
   - первой фазы определения размера буфера (передача нулевого указателя/размера 0 и проверка перехвата ожидаемого кода `ERROR_INSUFFICIENT_BUFFER`);
   - второй фазы считывания привилегий в буфер и ее независимого пути отказа (`TokenPrivilegeQueryFailure`).
3. **Сохранение и передача неизменного PreviousState при восстановлении**:
   В соответствии с нормативной семантикой Win32, функция `AdjustTokenPrivileges` возвращает исходное состояние привилегий вызывающего процесса в параметре `PreviousState`. Тестовый шов и логика адаптера ОБЯЗАНЫ захватывать полную структуру `PreviousState` (`TOKEN_PRIVILEGES`) и передавать ее без искажения и без редукции к простому булеву флагу при компенсирующем восстановлении привилегий после сбоя запроса выключения.
4. **Детерминированная симуляция граничных исходов Win32**:
   Тестовый дублер (`FakeWindowsPowerPort`) обязан поддерживать управляемую симуляцию следующих состояний:
   - возврат `ERROR_NOT_ALL_ASSIGNED` (код 1300) от `AdjustTokenPrivileges` (PWR-07);
   - нативный сбой системного вызова `AdjustTokenPrivileges` при включении привилегии (PWR-08);
   - сбой системного вызова восстановления привилегии (PWR-10);
   - ошибка `ERROR_SHUTDOWN_IN_PROGRESS` (код 1115) от `InitiateSystemShutdownExW` (PWR-11);
   - общая ошибка системного вызова выключения `ShutdownRequestFailure` (PWR-09);
   - успешный возврат и принятие запроса `REQUEST_ACCEPTED` (PWR-12).
5. **Инкапсуляция небезопасного кода (Memory Safety & Handle Ownership)**:
   Все операции с небезопасными указателями Win32, синтаксический разбор сырых буферов токена и управление временем жизни нативных дескрипторов (`HANDLE` через RAII `SafeHandle`) обязаны оставаться строго внутри границы реализации `crates/windows-platform`.
6. **Совместимость с крейтом windows 0.62.2**:
   Интерфейс тестового шва обязан быть полностью совместим с типами и сигнатурами крейта `windows` 0.62.2.
7. **Сохранение внешних контрактов и направления зависимостей**:
   Внутренний тестовый шов не экспортируется в публичный API рантайма службы. Публичный контракт `palka_service::runtime::PowerController` остается строго неизменным (метод `check_privileges()` в него не добавляется). Зависимость сохраняет строгое направление `palka-service -> palka-windows-platform`.

### 6.3. Зависимости windows-rs (версия 0.62.2)
Для реализации жизненного цикла управления питанием в `crates/windows-platform/Cargo.toml` добавляются следующие **дополнительные** фичи:
- `Win32_System_Shutdown` (для `InitiateSystemShutdownExW`)
- `Win32_System_Threading` (для `OpenProcessToken`, `GetCurrentProcess`)

Существующие фичи крейта (`Win32_Foundation`, `Win32_Security`, `Win32_Storage_FileSystem` и др.) полностью сохраняются. Замена крейта `windows` на `windows-sys` категорически запрещена.

Канонические пространства имен Win32 API:
- `windows::Win32::System::Shutdown::InitiateSystemShutdownExW`
- `windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken}`
- `windows::Win32::Security::{LookupPrivilegeValueW, AdjustTokenPrivileges, GetTokenInformation, TOKEN_PRIVILEGES, SE_SHUTDOWN_NAME}`
- `windows::Win32::Foundation::{CloseHandle, HANDLE, BOOL, LUID}`

---

## 7. Нормативная матрица верификации (PWR-01 .. PWR-38)

Матрица охватывает все аспекты верификации подсистемы управления питанием.

| ID | Тип | Описание сценария |
|---|---|---|
| **PWR-01** | Unit | Вызов `initiate_shutdown` передает в шов точные параметры: `timeout=0`, `force=true`, `reboot=false`, `reason=0x80000000`, null machine, null message |
| **PWR-02** | Unit | Сбой `OpenProcessToken` при проверке готовности возвращает `OpenProcessTokenFailure { win32_code }` |
| **PWR-03** | Unit | Сбой `LookupPrivilegeValueW` возвращает `LookupPrivilegeFailure { win32_code }` |
| **PWR-04** | Unit | Первый вызов `GetTokenInformation` с размером 0 корректно перехватывает ожидаемый `ERROR_INSUFFICIENT_BUFFER` |
| **PWR-05** | Unit | Второй вызов `GetTokenInformation` при сбое возвращает `TokenPrivilegeQueryFailure { win32_code }` |
| **PWR-06** | Unit | Отсутствие `SeShutdownPrivilege` в массиве токена возвращает `PrivilegeNotAssigned` |
| **PWR-07** | Unit | `AdjustTokenPrivileges` при возврате `ERROR_NOT_ALL_ASSIGNED` возвращает `PrivilegeNotAssigned` |
| **PWR-08** | Unit | Нативный сбой `AdjustTokenPrivileges` при включении возвращает `AdjustPrivilegeFailure { win32_code }` |
| **PWR-09** | Unit | Сбой вызова выключения при успешном откате привилегии возвращает исходную ошибку `ShutdownRequestFailure` |
| **PWR-10** | Unit | Сбой вызова выключения при сбое отката привилегии возвращает приоритетную `PrivilegeRestoreFailure` |
| **PWR-11** | Unit | Сбой выключения с `ERROR_SHUTDOWN_IN_PROGRESS` (1115) возвращает `ShutdownAlreadyInProgress` |
| **PWR-12** | Unit | Успешный вызов возвращает `Ok(())` (`REQUEST_ACCEPTED`), откат привилегии не вызывается |
| **PWR-13** | Unit | Вызов на не-Windows платформе возвращает `UnsupportedPlatform` |
| **PWR-14** | Static | RAII SafeHandle закрывает токен процесса во всех путях возврата без утечек дескрипторов |
| **PWR-15** | Static | Запрет вызова внешних процессов и командных процессоров (`cmd.exe`, `powershell.exe`, `shutdown.exe`) |
| **PWR-16** | Static | Дополнительные фичи windows-rs ограничены `Win32_System_Shutdown` и `Win32_System_Threading` |
| **PWR-17** | Static | Отсутствие вызова `AbortSystemShutdownW` в производственном пути кода |
| **PWR-18** | Static | Переменная `PreviousState` корректно передается и захватывается при вызове `AdjustTokenPrivileges` |
| **PWR-19** | Static | Отсутствие обязательного вызова восстановления привилегии после успешного принятия запроса в V1 |
| **PWR-20** | Runtime | Сохранение состояния `Executing` предшествует вызову платформенного эффекта выключения |
| **PWR-21** | Runtime | Просроченное действие `ShutdownComputer` при восстановлении службы переходит в `Missed` без вызова питания |
| **PWR-22** | Runtime | Отмена таймера выключения при дедлайне или после него (`remaining <= 0`) строго запрещена |
| **PWR-23** | Runtime | Сбой питания переводит действие в `Failed { reason }`, здоровье в `Degraded` и ставит запись в outbox |
| **PWR-24** | Runtime | Успех питания переводит действие в `Completed` и удаляет его из `active_actions` |
| **PWR-25** | Integration | Токен процесса службы `LocalSystem` в реальной Windows содержит назначенную `SeShutdownPrivilege` |
| **PWR-26** | Integration | Включение `SeShutdownPrivilege` успешно выполняется реальной службой в Session 0 |
| **PWR-27** | Integration | Вызов `InitiateSystemShutdownExW` из службы в Session 0 переводит операцию в `REQUEST_ACCEPTED` |
| **PWR-28** | Integration | Поведение процесса службы в период между `REQUEST_ACCEPTED` и физическим завершением ОС |
| **PWR-29** | Integration | Выключение успешно инициируется при отсутствии залогиненного интерактивного пользователя |
| **PWR-30** | Integration | Выключение корректно инициируется при нескольких активных сеансах (Fast User Switching / RDP) |
| **PWR-31** | Integration | Параметр `bForceAppsClosed=TRUE` закрывает блокнот с несохраненными данными без показа диалога |
| **PWR-32** | Integration | Поведение операционной системы при наличии зависшего/неотвечающего интерактивного процесса |
| **PWR-33** | Integration | Реальный возврат `ERROR_SHUTDOWN_IN_PROGRESS` при повторной попытке выключения |
| **PWR-34** | Integration | Подтверждение полного отключения питания рабочей станции, а не перезагрузки (`bRebootAfterShutdown=FALSE`) |
| **PWR-35** | Integration | Поведение службы при неожиданном незавершении физического выключения ОС после рестарта |
| **PWR-36** | Integration | Непривилегированный пользователь Ребенок не имеет прав отменить выключение после дедлайна |
| **PWR-37** | Integration | Запись в журнале событий Windows (Event ID 1074, User32) фиксирует причину `0x80000000` (Other Planned) |
| **PWR-38** | Integration | При `dwTimeout=0` на рабочем столе пользователя не отображается обратный отсчет Windows |

### Итоги классификации матрицы:
- **UNIT_PROVEN**: 13
- **STATIC_IMPLEMENTATION_PROVEN**: 6
- **EXISTING_RUNTIME_PROVEN**: 5
- **WINDOWS_INTEGRATION_REQUIRED**: 14
- **FAIL_COUNT**: 0
- **Всего сценариев**: 38

---

## 8. Граница будущей реализации кода

Создание настоящего документа **НЕ АВТОРИЗУЕТ** немедленного редактирования кода. Реализация разрешена только в рамках отдельного жизненного цикла `WINDOWS-POWER-CONTROLLER IMPLEMENTATION`.

### Допустимый объем изменений при последующей реализации:
- `crates/windows-platform/Cargo.toml` (добавление фичей `Win32_System_Shutdown`, `Win32_System_Threading`);
- `crates/windows-platform/src/lib.rs` (экспорт модуля `power`);
- `crates/windows-platform/src/power.rs` (публичный тип, порт, ошибки, тесты);
- `crates/windows-platform/src/power_windows.rs` (Win32 вызовы и SafeHandle);
- `crates/service/src/lib.rs` (экспорт сервисного адаптера);
- `crates/service/src/windows_power_controller.rs` (привязка к `palka_service::runtime::PowerController`).

### Порядок разблокировки docs/017:
Завершение реализации `WINDOWS-POWER-CONTROLLER` устранит соответствующий блокер для `ProductionRuntimeFactory`, однако интеграция SCM исполняемого файла (`docs/017`) останется заблокированной зависимостью `IdSource` (`PRODUCTION_ID_SOURCE`). Следующим шагом после управления питанием должна стать реализация производственного генератора идентификаторов.

---

## 9. Нормативные технические первоисточники (Microsoft Learn)

1. [InitiateSystemShutdownExW function (winreg.h)](https://learn.microsoft.com/en-us/windows/win32/api/winreg/nf-winreg-initiatesystemshutdownexw)
2. [AdjustTokenPrivileges function (securitybaseapi.h)](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-adjusttokenprivileges)
3. [GetTokenInformation function (securitybaseapi.h)](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-gettokeninformation)
4. [LookupPrivilegeValueW function (winbase.h)](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-lookupprivilegevaluew)
5. [OpenProcessToken function (processthreadsapi.h)](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocesstoken)
6. [System Shutdown Reason Codes](https://learn.microsoft.com/en-us/windows/win32/shutdown/system-shutdown-reason-codes)
7. [Privilege Constants (Authorization)](https://learn.microsoft.com/en-us/windows/win32/secauthz/privilege-constants)
