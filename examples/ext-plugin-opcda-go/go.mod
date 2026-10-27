module rs-iot/examples/ext-plugin-opcda-go

go 1.20

require github.com/huskar-t/opcda v0.4.1

require golang.org/x/sys v0.30.0 // indirect

// 本地开发：opcda 源码在 rs-iot 同级（E:\github.com\opcda），相对本 go.mod 是 ../../../opcda。
// 正式构建可改为远程 require（go get github.com/huskar-t/opcda@v0.4.1）并删除 replace。
replace github.com/huskar-t/opcda => ../../../opcda
