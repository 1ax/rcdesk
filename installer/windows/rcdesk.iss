; Установщик rcdesk для Windows (Inno Setup 6.3+, слайс 4.3b).
;
; Сборка (CI делает то же самое, см. .github/workflows/build-host.yml):
;   iscc /DAppVersion=0.1.0 /DSourceDir=C:\путь\к\target\release /OC:\out installer\windows\rcdesk.iss
; AppVersion -- версия из Cargo.toml, SourceDir -- каталог с rcdesk-host.exe и
; rcdesk-agent.exe. Результат: <каталог /O>\rcdesk-setup.exe.
;
; Файл сохранён в UTF-8 с BOM: без BOM Inno читает скрипт как ANSI и ломает
; русские строки. Не убирать BOM при правке.
;
; Тихая установка и LAN-хаб (D47-5b): /HUB -- этот компьютер становится
; LAN-хабом (нужны права администратора, полный режим включается сам);
; /SERVER=<адрес> -- подключаться к LAN-хабу (192.168.1.10, hub:9443 или
; полный wss://…/ws). Без обоих параметров тихая установка settings.json не
; трогает (так работает самообновление).

#ifndef AppVersion
  #error Передайте версию: iscc /DAppVersion=X.Y.Z
#endif
#ifndef SourceDir
  #error Передайте каталог с exe: iscc /DSourceDir=...\target\release
#endif

[Setup]
; Фиксированный GUID приложения: по нему установщик находит прошлую
; установку (обновление поверх, удаление). Менять НЕЛЬЗЯ -- иначе обновление
; поставит вторую копию рядом.
AppId={{B256AA75-6051-4CC5-9016-42EBB04A7EB8}
AppName=rcdesk
AppVersion={#AppVersion}
AppPublisher=rcdesk
AppPublisherURL=https://rcdesk.app
; Inno требует четыре числа; 3-частную версию из Cargo дополняем нулём.
VersionInfoVersion={#AppVersion}.0
VersionInfoProductName=rcdesk
SetupIconFile=rcdesk.ico
UninstallDisplayIcon={app}\rcdesk-agent.exe
OutputBaseFilename=rcdesk-setup
WizardStyle=modern
DisableProgramGroupPage=yes
Compression=lzma2
SolidCompression=yes
; Только 64-бит (захват и MFT -- под x64; стенд -- Windows 10 x64).
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; По умолчанию просим администратора («для всех», Program Files, служба);
; в диалоге выбора можно «только для меня» -- тогда %LOCALAPPDATA%\Programs\rcdesk
; и без UAC. Диалог показывается всегда: без него обычному пользователю
; пришлось бы запускать установщик от администратора ради режима «для меня».
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog
DefaultDirName={autopf}\rcdesk
; Закрытие приложений делаем сами в PrepareToInstall (служба + taskkill).
; Менеджер перезапуска Inno про службу ничего не знает и задавал бы вопросы
; про rcdesk-agent.exe, который мы всё равно закроем.
CloseApplications=no

[Languages]
; Русский первым -- он же запасной, если язык системы не распознан.
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
russian.ModePageCaption=Режим работы
russian.ModePageDescription=Как rcdesk будет работать на этом компьютере
russian.ModePageSubCaption=Выберите режим. Его можно сменить, запустив установщик ещё раз.
russian.ModeNormal=Обычный: агент в трее, запуск при входе в Windows
russian.ModeFull=Полный: служба Windows (окна администратора, работа до входа в систему)
russian.ServiceNeedsAdmin=Служба rcdesk уже установлена для всех пользователей. Чтобы обновить или изменить её, запустите установщик ещё раз и выберите установку для всех пользователей.
russian.ServiceInstallFailed=Не удалось установить службу rcdesk. Подробности -- в файле журнала службы (путь ниже).%nПрограмма установлена, но полный режим не работает: запустите установщик ещё раз.
russian.StartMenuShortcut=rcdesk
russian.ServerPageCaption=Сервер
russian.ServerPageDescription=Через какой сервер rcdesk будет соединять компьютеры
russian.ServerPageSubCaption=Выберите сервер. Его можно сменить, запустив установщик ещё раз.
russian.ServerCloud=Облако rcdesk.app (нужен интернет)
russian.ServerHub=Этот компьютер -- LAN-хаб (сеть без интернета, только полный режим)
russian.ServerClient=Подключаться к LAN-хабу
russian.HubNeedsFull=Хаб работает только в полном режиме (служба Windows). Вернитесь и выберите полный режим.
russian.HubAddrCaption=Адрес хаба
russian.HubAddrDescription=Где в сети находится компьютер-хаб
russian.HubAddrSubCaption=IP или имя компьютера-хаба, например 192.168.1.10
russian.HubAddrLabel=Адрес хаба:
russian.HubAddrInvalid=Введите адрес хаба без пробелов и кавычек, например 192.168.1.10.
english.ModePageCaption=Operating mode
english.ModePageDescription=How rcdesk will run on this computer
english.ModePageSubCaption=Choose a mode. You can change it later by running the installer again.
english.ModeNormal=Normal: tray agent, starts at Windows sign-in
english.ModeFull=Full: Windows service (administrator windows, works before sign-in)
english.ServiceNeedsAdmin=The rcdesk service is already installed for all users. To update or change it, run the installer again and choose installation for all users.
english.ServiceInstallFailed=Could not install the rcdesk service. Details are in the service log (path below).%nThe program is installed, but full mode is not working: run the installer again.
english.StartMenuShortcut=rcdesk
english.ServerPageCaption=Server
english.ServerPageDescription=Which server rcdesk uses to connect computers
english.ServerPageSubCaption=Choose a server. You can change it later by running the installer again.
english.ServerCloud=rcdesk.app cloud (internet required)
english.ServerHub=This computer is the LAN hub (network without internet, full mode only)
english.ServerClient=Connect to a LAN hub
english.HubNeedsFull=The hub works only in full mode (Windows service). Go back and choose full mode.
english.HubAddrCaption=Hub address
english.HubAddrDescription=Where the hub computer is on the network
english.HubAddrSubCaption=IP or name of the hub computer, for example 192.168.1.10
english.HubAddrLabel=Hub address:
english.HubAddrInvalid=Enter the hub address without spaces or quotes, for example 192.168.1.10.

[Files]
; Оба exe всегда рядом: `rcdesk-host autostart` ищет rcdesk-agent.exe в своём каталоге.
Source: "{#SourceDir}\rcdesk-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\rcdesk-agent.exe"; DestDir: "{app}"; Flags: ignoreversion

[InstallDelete]
; Переход обычный -> полный: ярлык больше не нужен (агент при службе сам выходит).
Type: files; Name: "{autoprograms}\rcdesk.lnk"; Check: not IsNormalMode

[Icons]
Name: "{autoprograms}\{cm:StartMenuShortcut}"; Filename: "{app}\rcdesk-agent.exe"; Check: IsNormalMode

; [Run] не используется: автозапуск и запуск агента делаются в [Code]
; (CurStepChanged, ExecAsOriginalUser) -- см. комментарии там.

[Code]
const
  ServiceKey = 'SYSTEM\CurrentControlSet\Services\rcdesk';
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  FirewallRule = 'rcdesk hub';

var
  ModePage: TInputOptionWizardPage;
  ServerPage: TInputOptionWizardPage;
  HubAddrPage: TInputQueryWizardPage;
  // Параметры тихой установки (/HUB, /SERVER=); см. InitializeSetup.
  ParamHub: Boolean;
  ParamServer: String;
  // Писать ли settings.json и трогать правило брандмауэра (см. InitializeWizard).
  SettingsTouched: Boolean;

// Служба есть в SCM -- по ключу реестра, а не по `rcdesk-host service status`:
// status возвращает код 0 и для «не установлена», и для «работает» (различие
// только в тексте на stdout, локализованном), а ключ читается без прав
// администратора и без exe (на первой установке exe в {app} ещё нет).
function ServiceExists: Boolean;
begin
  Result := RegKeyExists(HKLM, ServiceKey);
end;

// Путь к exe в службе (ImagePath: `"<exe>" service run --server ...`) указывает
// на rcdesk-host.exe из текущего {app}. Если каталог сменили (/DIR=, повторная
// установка в другое место), служба продолжила бы запускать старую копию.
function ServicePathMatches: Boolean;
var
  ImagePath, Expected: String;
begin
  Result := False;
  if RegQueryStringValue(HKLM, ServiceKey, 'ImagePath', ImagePath) then
  begin
    Expected := Lowercase('"' + ExpandConstant('{app}\rcdesk-host.exe') + '"');
    Result := Pos(Expected, Lowercase(ImagePath)) = 1;
  end;
end;

// В ImagePath есть `--server`: старые установки несли
// `--server wss://rcdesk.app/ws`. Флаг в binpath перебивает settings.json,
// поэтому такую службу пересоздаём без него.
function ServiceHasServerFlag: Boolean;
var
  ImagePath: String;
begin
  Result := RegQueryStringValue(HKLM, ServiceKey, 'ImagePath', ImagePath) and
    (Pos('--server', Lowercase(ImagePath)) > 0);
end;

function IsFullMode: Boolean;
begin
  Result := IsAdminInstallMode and (ModePage <> nil) and ModePage.Values[1];
end;

function IsNormalMode: Boolean;
begin
  Result := not IsFullMode;
end;

// Запуск от имени того, кто запустил установщик (до UAC): запись в
// HKCU\...\Run должна лечь ему, а не администратору из окна UAC, и агент должен
// подняться в его сеансе. Это `ExecAsOriginalUser` (то же, что флаг
// runasoriginaluser в [Run]); [Run] не используем -- порядок его записей
// относительно ssPostInstall в документации не гарантирован, а нам нужно
// строго: служба -> автозапуск -> агент. Ограничение Inno: если установщик
// запущен «от имени администратора» (правая кнопка) или из повышенного
// процесса, исходной учётки нет -- HKCU и агент будут у этого администратора.
function RunHostAsUser(const Params: String): Boolean;
var
  Code: Integer;
begin
  Result := ExecAsOriginalUser(ExpandConstant('{app}\rcdesk-host.exe'), Params, '', SW_HIDE,
    ewWaitUntilTerminated, Code) and (Code = 0);
  Log(Format('(user) rcdesk-host %s -> %d', [Params, Code]));
end;

// true, если rcdesk-host.exe из {app} отработал с кодом 0 (от имени установщика).
function RunHost(const Params: String): Boolean;
var
  Code: Integer;
begin
  Result := Exec(ExpandConstant('{app}\rcdesk-host.exe'), Params, '', SW_HIDE,
    ewWaitUntilTerminated, Code) and (Code = 0);
  Log(Format('rcdesk-host %s -> %d', [Params, Code]));
end;

// Закрывает все rcdesk-agent.exe. В режиме «для всех» это закрывает агентов
// всех вошедших пользователей -- приемлемо: иначе exe заняты и не заменятся.
// Результат игнорируется (агента может не быть).
procedure KillAgents;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/IM rcdesk-agent.exe /F', '', SW_HIDE,
    ewWaitUntilTerminated, Code);
  Log(Format('taskkill rcdesk-agent.exe -> %d', [Code]));
end;

// Адрес хаба допустим, если он непустой и без пробелов, табуляции, кавычек и
// обратной косой (строка попадает в JSON как есть).
function HubAddressValid(const S: String): Boolean;
var
  T: String;
begin
  T := Trim(S);
  Result := (T <> '') and (Pos(' ', T) = 0) and (Pos(#9, T) = 0) and
    (Pos('"', T) = 0) and (Pos('''', T) = 0) and (Pos('\', T) = 0);
end;

// Приводит введённый адрес хаба к виду wss://host:port/path:
//   192.168.1.10   -> wss://192.168.1.10:8443/ws
//   hub:9443       -> wss://hub:9443/ws
//   wss://h:1/ws   -> wss://h:1/ws (без изменений)
// Нет схемы -- wss://; нет порта в host[:port] -- :8443; нет пути -- /ws.
// IPv6-литералы не поддерживаются (в них есть ':').
function NormalizeHubAddress(S: String): String;
var
  Scheme, Rest, HostPort, UrlPath: String;
  P, Slash: Integer;
begin
  Result := Trim(S);
  if Pos('://', Result) = 0 then
    Result := 'wss://' + Result;
  P := Pos('://', Result);
  Scheme := Copy(Result, 1, P + 2);
  Rest := Copy(Result, P + 3, Length(Result));
  Slash := Pos('/', Rest);
  if Slash = 0 then
  begin
    HostPort := Rest;
    UrlPath := '';
  end
  else
  begin
    HostPort := Copy(Rest, 1, Slash - 1);
    UrlPath := Copy(Rest, Slash, Length(Rest));
  end;
  if Pos(':', HostPort) = 0 then
    HostPort := HostPort + ':8443';
  if UrlPath = '' then
    UrlPath := '/ws';
  Result := Scheme + HostPort + UrlPath;
end;

// Каталог settings.json: полный режим -- общий (читает служба), обычный --
// профиль (читает агент). Известное ограничение: при установке «для всех» в
// обычном режиме {localappdata} -- профиль повышенной учётки (того, кто ввёл
// пароль в UAC), а не исходного пользователя.
function SettingsDir: String;
begin
  if IsFullMode then
    Result := ExpandConstant('{commonappdata}\rcdesk')
  else
    Result := ExpandConstant('{localappdata}\rcdesk');
end;

// Есть ли в командной строке параметр Name (без учёта регистра).
function HasParam(const Name: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), Name) = 0 then
      Result := True;
end;

// Тихие /HUB и /SERVER=: проверяем до показа мастера. Ошибка параметров --
// выход без установки, а не молча облако.
function InitializeSetup: Boolean;
begin
  Result := True;
  ParamHub := HasParam('/HUB');
  ParamServer := Trim(ExpandConstant('{param:SERVER|}'));
  if ParamHub and (ParamServer <> '') then
  begin
    Log('/HUB и /SERVER= взаимоисключающие');
    Result := False;
  end
  else if ParamHub and (not IsAdminInstallMode) then
  begin
    Log('/HUB требует прав администратора (хаб = служба Windows)');
    Result := False;
  end
  else if (ParamServer <> '') and (not HubAddressValid(ParamServer)) then
  begin
    Log('/SERVER=: недопустимый адрес: ' + ParamServer);
    Result := False;
  end;
end;

// Предвыбор страницы «Сервер» по settings.json каталога, который соответствует
// предвыбору режима (служба есть -- общий, иначе профиль). Разбор простой:
// пробелы и переводы строк убираем, ищем "hub":true и "server":"...".
procedure PreselectServer(var Variant: Integer; var Addr: String);
var
  Dir, S, Rest: String;
  Raw: AnsiString;
  P, Q: Integer;
begin
  Variant := 0;
  Addr := '';
  if ServiceExists then
    Dir := ExpandConstant('{commonappdata}\rcdesk')
  else
    Dir := ExpandConstant('{localappdata}\rcdesk');
  if not LoadStringFromFile(Dir + '\settings.json', Raw) then
    Exit;
  S := Raw;
  StringChangeEx(S, ' ', '', True);
  StringChangeEx(S, #9, '', True);
  StringChangeEx(S, #13, '', True);
  StringChangeEx(S, #10, '', True);
  // hub действует только в машинном каталоге и только для администратора.
  if (Pos('"hub":true', S) > 0) and ServiceExists and IsAdminInstallMode then
    Variant := 1
  else
  begin
    P := Pos('"server":"', S);
    if P > 0 then
    begin
      Rest := Copy(S, P + 10, Length(S));
      Q := Pos('"', Rest);
      if Q > 0 then
      begin
        Variant := 2;
        Addr := Copy(Rest, 1, Q - 1);
      end;
    end;
  end;
end;

procedure InitializeWizard;
var
  Variant: Integer;
  Addr: String;
begin
  // После выбора каталога. Исключительные радиокнопки; по умолчанию обычный
  // режим, а при переустановке поверх службы -- полный (не сбрасываем режим).
  ModePage := CreateInputOptionPage(wpSelectDir,
    CustomMessage('ModePageCaption'), CustomMessage('ModePageDescription'),
    CustomMessage('ModePageSubCaption'), True, False);
  ModePage.Add(CustomMessage('ModeNormal'));
  ModePage.Add(CustomMessage('ModeFull'));
  ModePage.Values[0] := not ServiceExists;
  ModePage.Values[1] := ServiceExists;

  // Сервер: после страницы режима; страница адреса -- только для «клиента».
  ServerPage := CreateInputOptionPage(ModePage.ID,
    CustomMessage('ServerPageCaption'), CustomMessage('ServerPageDescription'),
    CustomMessage('ServerPageSubCaption'), True, False);
  ServerPage.Add(CustomMessage('ServerCloud'));
  ServerPage.Add(CustomMessage('ServerHub'));
  ServerPage.Add(CustomMessage('ServerClient'));
  HubAddrPage := CreateInputQueryPage(ServerPage.ID,
    CustomMessage('HubAddrCaption'), CustomMessage('HubAddrDescription'),
    CustomMessage('HubAddrSubCaption'));
  HubAddrPage.Add(CustomMessage('HubAddrLabel'), False);

  PreselectServer(Variant, Addr);
  // Параметры командной строки сильнее файла.
  if ParamHub then
  begin
    Variant := 1;
    // Хаб = служба: полный режим принудительно (InitializeSetup уже
    // проверил права администратора).
    ModePage.Values[0] := False;
    ModePage.Values[1] := True;
  end
  else if ParamServer <> '' then
  begin
    Variant := 2;
    Addr := ParamServer;
  end;
  ServerPage.Values[0] := (Variant = 0);
  ServerPage.Values[1] := (Variant = 1);
  ServerPage.Values[2] := (Variant = 2);
  HubAddrPage.Values[0] := Addr;

  // Интерактивно пишем по выбору на странице; тихо -- только если задан
  // /HUB или /SERVER= (иначе самообновление затёрло бы настройки).
  SettingsTouched := (not WizardSilent) or ParamHub or (ParamServer <> '');
end;

// Режим «только для меня» не умеет ставить службу (нужен администратор) --
// страницы нет, режим всегда обычный.
function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := (ModePage <> nil) and (PageID = ModePage.ID) and (not IsAdminInstallMode);
  // Страница адреса нужна только для варианта «подключаться к хабу».
  if (HubAddrPage <> nil) and (PageID = HubAddrPage.ID) then
    Result := not ServerPage.Values[2];
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (ServerPage <> nil) and (CurPageID = ServerPage.ID) then
  begin
    // Хаб -- это служба: нужен полный режим (а он -- права администратора).
    if ServerPage.Values[1] and (not IsFullMode) then
    begin
      MsgBox(CustomMessage('HubNeedsFull'), mbError, MB_OK);
      Result := False;
    end;
  end
  else if (HubAddrPage <> nil) and (CurPageID = HubAddrPage.ID) then
  begin
    if not HubAddressValid(HubAddrPage.Values[0]) then
    begin
      MsgBox(CustomMessage('HubAddrInvalid'), mbError, MB_OK);
      Result := False;
    end;
  end;
end;

// Перед заменой файлов: остановить службу и закрыть агентов.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Result := '';
  if ServiceExists then
  begin
    if not IsAdminInstallMode then
    begin
      // Остановить/удалить службу без администратора нельзя.
      Result := CustomMessage('ServiceNeedsAdmin');
      Exit;
    end;
    // Службу останавливает rcdesk-host из каталога установки (ждёт до 10 с);
    // если его там нет (каталог сменили), падаем на `sc stop` и ждём вслепую.
    if FileExists(ExpandConstant('{app}\rcdesk-host.exe')) then
      RunHost('service stop')
    else
    begin
      Exec(ExpandConstant('{sys}\sc.exe'), 'stop rcdesk', '', SW_HIDE, ewWaitUntilTerminated, Code);
      Log(Format('sc stop rcdesk -> %d', [Code]));
      Sleep(3000);
    end;
  end;
  KillAgents;
end;

procedure InstallService;
begin
  // Сервер -- по умолчанию (без --server). Ошибка -- единственная, которую
  // показываем пользователю. device.json/access.json install копирует из
  // %LOCALAPPDATA% учётки, под которой работает установщик.
  if not RunHost('service install') then
    MsgBox(CustomMessage('ServiceInstallFailed') + #13#10 + '%ProgramData%\rcdesk\logs\service.log', mbError, MB_OK);
end;

// settings.json пишется целиком (хост его только читает), ASCII без BOM.
//   облако -- `{}`, но только если файл уже есть (новый не создаём);
//   хаб    -- {"hub": true};  клиент -- {"server": "<адрес>"}.
// Вызывать ДО установки/старта службы: служба читает файл при старте.
procedure WriteSettings;
var
  Dir, FileName, Content: String;
begin
  if not SettingsTouched then
  begin
    Log('settings.json не трогаем (тихая установка без /HUB и /SERVER=)');
    Exit;
  end;
  Dir := SettingsDir;
  FileName := Dir + '\settings.json';
  if ServerPage.Values[1] and IsFullMode then
    Content := '{"hub": true}'
  else if ServerPage.Values[2] then
    Content := '{"server": "' + NormalizeHubAddress(HubAddrPage.Values[0]) + '"}'
  else
  begin
    if not FileExists(FileName) then
      Exit;
    Content := '{}';
  end;
  ForceDirectories(Dir);
  if SaveStringToFile(FileName, Content + #13#10, False) then
    Log('settings.json записан: ' + FileName + ' ' + Content)
  else
    Log('не удалось записать ' + FileName);
end;

// netsh в скрытом окне; код -- в журнал. Ошибка «нет правила» не ошибка.
procedure RunNetsh(const Params: String);
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\netsh.exe'), Params, '', SW_HIDE, ewWaitUntilTerminated, Code);
  Log(Format('netsh %s -> %d', [Params, Code]));
end;

// Правило входящего TCP 8443 для хаба. Только с правами администратора и
// только когда settings пишутся (тихое обновление правило не трогает).
procedure ApplyFirewall;
begin
  if (not SettingsTouched) or (not IsAdminInstallMode) then
    Exit;
  RunNetsh('advfirewall firewall delete rule name="' + FirewallRule + '"');
  if ServerPage.Values[1] and IsFullMode then
    RunNetsh('advfirewall firewall add rule name="' + FirewallRule +
      '" dir=in action=allow protocol=TCP localport=8443 program="' +
      ExpandConstant('{app}\rcdesk-host.exe') + '" profile=any');
end;

procedure LaunchAgent;
var
  Code: Integer;
begin
  // nowait: агент живёт, пока вошёл пользователь.
  ExecAsOriginalUser(ExpandConstant('{app}\rcdesk-agent.exe'), '', '', SW_SHOWNORMAL,
    ewNoWait, Code);
  Log(Format('(user) rcdesk-agent.exe -> %d', [Code]));
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    // Первым делом: служба читает settings.json при старте.
    WriteSettings;
    ApplyFirewall;
  end;

  if (CurStep = ssPostInstall) and IsAdminInstallMode then
  begin
    if IsFullMode then
    begin
      if not ServiceExists then
        InstallService
      else if ServicePathMatches and (not ServiceHasServerFlag) then
        RunHost('service start')
      else
      begin
        // Каталог сменили или в binpath остался `--server` (старые установки
        // несли `--server wss://rcdesk.app/ws`, флаг перебивает settings.json):
        // служба смотрит не туда. Пересоздаём без флага -- пользовательский
        // --server (если задавали) сбрасывается; данные в %ProgramData%\rcdesk
        // сохраняются.
        RunHost('service uninstall');
        InstallService;
      end;
    end
    else if ServiceExists then
      // Полный -> обычный: служба больше не нужна. Данные не трогаем.
      RunHost('service uninstall');
  end;

  if CurStep = ssPostInstall then
  begin
    if IsFullMode then
      // Агент при работающей службе сам выходит, запись в Run лишняя.
      RunHostAsUser('autostart off')
    else
    begin
      // restore: не включать, если пользователь выключил автозапуск в меню агента.
      RunHostAsUser('autostart restore');
      LaunchAgent;
    end;
  end;
end;

// Удаление: служба, агенты, автозапуск -- до удаления файлов (usUninstall
// идёт раньше). [UninstallRun] не подходит: флаг runasoriginaluser в
// документации Inno Setup помечен «valid only in a [Run] section».
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Code: Integer;
  RunValue: String;
begin
  if CurUninstallStep = usUninstall then
  begin
    if ServiceExists then
    begin
      Exec(ExpandConstant('{app}\rcdesk-host.exe'), 'service uninstall', '', SW_HIDE,
        ewWaitUntilTerminated, Code);
      Log(Format('service uninstall -> %d', [Code]));
    end;
    Exec(ExpandConstant('{sys}\taskkill.exe'), '/IM rcdesk-agent.exe /F', '', SW_HIDE,
      ewWaitUntilTerminated, Code);
    // Правило брандмауэра хаба (без администратора netsh не сработает).
    if IsAdminInstallMode then
    begin
      Exec(ExpandConstant('{sys}\netsh.exe'),
        'advfirewall firewall delete rule name="' + FirewallRule + '"', '', SW_HIDE,
        ewWaitUntilTerminated, Code);
      Log(Format('netsh delete rule -> %d', [Code]));
    end;
    // Автозапуск: деинсталлятор работает под учёткой, с которой его запустили
    // (при «для всех» -- повышенной), поэтому чистим HKCU именно её. Записи
    // других пользователей остаются и указывают на несуществующий exe --
    // Windows молча её пропустит. Удаляем только если значение указывает на
    // наш каталог, чтобы не сломать автозапуск копии, распакованной из zip.
    if RegQueryStringValue(HKCU, RunKey, 'rcdesk', RunValue) and
       (Pos(Lowercase(ExpandConstant('{app}')), Lowercase(RunValue)) > 0) then
      RegDeleteValue(HKCU, RunKey, 'rcdesk');
    // Данные (%LOCALAPPDATA%\rcdesk, %ProgramData%\rcdesk) не удаляем никогда.
  end;
end;
