// Pure text generators for the router-waker setup scripts (D85): the owner
// pastes the result into a MikroTik terminal / OpenWrt shell. No DOM access.

/** Replaces characters that could break out of a script comment/string. The
 * name only lands in a comment, but we defend anyway. */
function safeName(name: string): string {
  return name.replace(/["\\${};\r\n]/g, " ");
}

/** Single-pass substitution, so a token/name containing "NAME" or "TOKEN"
 * is never substituted a second time. */
function fill(template: string, origin: string, token: string, name: string): string {
  const values: Record<string, string> = {
    ORIGIN: origin.replace(/\/+$/, ""),
    TOKEN: token,
    NAME: safeName(name),
  };
  return template.replace(/ORIGIN|TOKEN|NAME/g, (key) => values[key] ?? key);
}

const MIKROTIK = `# rcdesk: роутер-будильник «NAME»
# Интерфейс локальной сети — bridge; если у вас другой, поправьте interface=bridge.
/system script add name=rcdesk-wake policy=ftp,read,write,test source={
:local r
:do { :set r [/tool fetch url="ORIGIN/wake/poll" http-header-field="Authorization: Bearer TOKEN" output=user as-value] } on-error={ :set r "" }
:if ([:typeof $r] = "array") do={
  :local d ($r->"data")
  :if ([:len $d] > 0) do={
    :foreach m in=[:toarray $d] do={
      /tool wol mac=$m interface=bridge
      :log info "rcdesk wol $m"
    }
  }
}
}
/system scheduler add name=rcdesk-wake interval=10s policy=ftp,read,write,test on-event="/system script run rcdesk-wake"
`;

const OPENWRT = `# rcdesk: роутер-будильник «NAME»
# OpenWrt 25.12+ (apk). На 24.10 и старше: opkg update && opkg install curl etherwake
# Интерфейс локальной сети — br-lan; если у вас другой, поправьте -i br-lan.
apk update && apk add curl etherwake
cat > /usr/bin/rcdesk-wake <<'EOF'
#!/bin/sh
while true; do
  macs=$(curl -fsS -m 8 -H "Authorization: Bearer TOKEN" "ORIGIN/wake/poll") &&
  for m in $(echo "$macs" | tr ',' ' '); do
    etherwake -i br-lan "$m"
    logger -t rcdesk "wol $m"
  done
  sleep 10
done
EOF
chmod +x /usr/bin/rcdesk-wake
cat > /etc/init.d/rcdesk-wake <<'EOF'
#!/bin/sh /etc/rc.common
START=99
USE_PROCD=1
start_service() {
  procd_open_instance
  procd_set_param command /usr/bin/rcdesk-wake
  procd_set_param respawn
  procd_close_instance
}
EOF
chmod +x /etc/init.d/rcdesk-wake
/etc/init.d/rcdesk-wake enable
/etc/init.d/rcdesk-wake start
`;

export function mikrotikScript(origin: string, token: string, name: string): string {
  return fill(MIKROTIK, origin, token, name);
}

export function openwrtScript(origin: string, token: string, name: string): string {
  return fill(OPENWRT, origin, token, name);
}
