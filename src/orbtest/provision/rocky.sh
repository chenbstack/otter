## 装工具和防火墙软件
dnf install -y nmap-ncat iptables-nft nftables firewalld dnf-plugins-core cronie
## 装 Docker
dnf config-manager --add-repo https://download.docker.com/linux/centos/docker-ce.repo
dnf install -y docker-ce docker-ce-cli containerd.io
systemctl enable --now docker
## 防火墙前端装好但不启用
systemctl disable --now firewalld || true
## 预拉测试用镜像
docker pull openresty/openresty:alpine
