## 更新软件源
export DEBIAN_FRONTEND=noninteractive
apt-get update -y
## 装工具(没有 ufw / firewalld:Debian 默认就没有)
export DEBIAN_FRONTEND=noninteractive
apt-get install -y curl ca-certificates netcat-openbsd iptables nftables cron
## 装 Docker
curl -fsSL https://get.docker.com | sh
systemctl enable --now docker
## 预拉测试用镜像
docker pull openresty/openresty:alpine
