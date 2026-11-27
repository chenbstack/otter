## 软件源改到 archive(Debian 11 已结束支持,原镜像上的包已下架)
cat > /etc/apt/sources.list <<'EOF'
deb http://archive.debian.org/debian bullseye main contrib
deb http://archive.debian.org/debian-security bullseye-security main contrib
EOF
echo 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/99orbtest
## 装工具
export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y --no-install-recommends curl ca-certificates netcat-openbsd cron
