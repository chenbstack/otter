## 更新软件源
export DEBIAN_FRONTEND=noninteractive
apt-get update -y
## 装工具和防火墙软件
export DEBIAN_FRONTEND=noninteractive
apt-get install -y curl ca-certificates netcat-openbsd iptables nftables ufw cron
## 装 Docker
curl -fsSL https://get.docker.com | sh
systemctl enable --now docker
## 防火墙前端装好但不启用
ufw --force disable || true
## 预拉测试用镜像
docker pull openresty/openresty:alpine
