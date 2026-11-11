#!/bin/sh
# Сторож цепочки DOCKER в iptables (I-10, инциденты 2026-10-21 и 2026-11-11).
#
# Что-то на VPS время от времени сбрасывает таблицу filter. Работающие контейнеры
# этого не замечают, но любой новый или пересозданный контейнер не может
# опубликовать порт: `iptables: No chain/target/match by that name` -- деплой
# падает уже после удаления рабочего контейнера. Цепочки возвращает только
# перезапуск демона Docker.
#
# Скрипт ставится root'ом в /usr/local/sbin и запускается таймером systemd раз
# в минуту (см. infra/README.md). Пока цепочка на месте, он ничего не делает.
set -eu

# Docker остановлен намеренно или ещё не поднялся -- не вмешиваемся.
systemctl is-active --quiet docker || exit 0

if iptables -w 5 -t filter -nL DOCKER >/dev/null 2>&1; then
    exit 0
fi

# Снимок правил на момент пропажи: по нему и по времени ищется виновник.
snap="/var/log/docker-fw-heal.$(date +%Y%m%d-%H%M%S).rules"
iptables-save >"$snap" 2>/dev/null || true
logger -t docker-fw-heal "цепочка DOCKER пропала из filter, перезапускаю docker (снимок: $snap)"

systemctl restart docker

if iptables -w 5 -t filter -nL DOCKER >/dev/null 2>&1; then
    logger -t docker-fw-heal "цепочка DOCKER восстановлена"
else
    logger -t docker-fw-heal "ОШИБКА: после перезапуска docker цепочки DOCKER нет"
    exit 1
fi
