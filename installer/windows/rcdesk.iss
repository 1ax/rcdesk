; Установщик rcdesk для Windows (Inno Setup 6.3+, слайс 4.3b).
;
; Сборка (CI делает то же самое, см. .github/workflows/build-host.yml):
;   iscc /DAppVersion=0.1.0 /DSourceDir=C:\путь\к\target\release /OC:\out installer\windows\rcdesk.iss
; AppVersion -- версия из Cargo.toml, SourceDir -- каталог с rcdesk-host.exe и
; rcdesk-agent.exe. Результат: <каталог /O>\rcdesk-setup.exe.
;
; Файл сохранён в UTF-8 с BOM: без BOM Inno читает скрипт как ANSI и ломает
; русские строки. Не убирать BOM при правке.

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
english.ModePageCaption=Operating mode
english.ModePageDescription=How rcdesk will run on this computer
english.ModePageSubCaption=Choose a mode. You can change it later by running the installer again.
english.ModeNormal=Normal: tray agent, starts at Windows sign-in
english.ModeFull=Full: Windows service (administrator windows, works before sign-in)
english.ServiceNeedsAdmin=The rcdesk service is already installed for all users. To update or change it, run the installer again and choose installation for all users.
english.ServiceInstallFailed=Could not install the rcdesk service. Details are in the service log (path below).%nThe program is installed, but full mode is not working: run the installer again.
english.StartMenuShortcut=rcdesk

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

var
  ModePage: TInputOptionWizardPage;

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

procedure InitializeWizard;
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
end;

// Режим «только для меня» не умеет ставить службу (нужен администратор) --
// страницы нет, режим всегда обычный.
function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := (ModePage <> nil) and (PageID = ModePage.ID) and (not IsAdminInstallMode);
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
  if (CurStep = ssPostInstall) and IsAdminInstallMode then
  begin
    if IsFullMode then
    begin
      if not ServiceExists then
        InstallService
      else if ServicePathMatches then
        RunHost('service start')
      else
      begin
        // Каталог сменили: служба смотрит на старый exe. Пересоздаём -- при
        // этом пользовательский --server (если задавали) сбрасывается на
        // умолчание; данные в %ProgramData%\rcdesk сохраняются.
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
      RunHostAsUser('autostart on');
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
