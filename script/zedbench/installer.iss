; Compile with ISCC /DVersion=x.y.z /DBinaries=... /DZedResources=... /O... installer.iss
[Setup]
AppId=ZEDIUM-Off.Zed.Private
AppName=Zedbench
AppVersion={#Version}
DefaultDirName={localappdata}\Programs\Zedbench
DisableDirPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
OutputBaseFilename=zedium-{#Version}-windows-x86_64
Compression=lzma2/fast
CloseApplications=no
RestartApplications=no

[Files]
Source: "{#Binaries}\zedbench.exe"; DestDir: "{code:InstallRoot}"; DestName: "Zed.exe"; Flags: ignoreversion
Source: "{#Binaries}\cli.exe"; DestDir: "{code:InstallRoot}\bin"; DestName: "Zed.exe"; Flags: ignoreversion
Source: "{#ZedResources}\zed.sh"; DestDir: "{code:InstallRoot}\bin"; DestName: "zed"; Flags: ignoreversion
Source: "{#Binaries}\conpty.dll"; DestDir: "{code:InstallRoot}"; Flags: ignoreversion
Source: "{#Binaries}\OpenConsole.exe"; DestDir: "{code:InstallRoot}\x64"; Flags: ignoreversion
Source: "{#Binaries}\auto-update-helper.exe"; DestDir: "{app}\tools"; DestName: "auto_update_helper.exe"; Flags: ignoreversion

[InstallDelete]
Type: files; Name: "{userprograms}\ZEDIUM Private.lnk"

[Icons]
Name: "{userprograms}\Zedbench"; Filename: "{app}\Zed.exe"

[Code]
function Updating(): Boolean;
begin
  Result := WizardSilent() and (CompareText(ExpandConstant('{param:update}'), 'true') = 0);
end;

function InstallRoot(Param: String): String;
begin
  Result := ExpandConstant('{app}');
  if Updating() then Result := Result + '\install';
end;

procedure CurStepChanged(Step: TSetupStep);
begin
  if (Step = ssPostInstall) and Updating() then
    if not SaveStringToFile(ExpandConstant('{app}\updates\versions.txt'), '{#Version}' + #13#10, True) then
      RaiseException('Unable to mark private update as ready');
end;
