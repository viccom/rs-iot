//go:build windows

// rs-iot 外部采集插件：OPC DA（Go + huskar-t/opcda，Windows COM）。
//
// 经 sidecarClient bridge 接入 rs-iot：连 OPC DA Server（如 Matrikon.OPC.Simulation.1），
// 周期同步读 item → DataUp 推 rs-iot；收写命令 → opcda 写 item。
// 协议详见 ../../../docs/external-plugin-protocol.md。
//
// 用法：
//
//	ext-opcda.exe --inst <id> --token <token> --bridge-host 127.0.0.1 --bridge-port 8781 \
//	  --prog-id Matrikon.OPC.Simulation.1 --opc-host localhost --period 2
//
// OPC items 来自 rs-iot ConfigPush（devices[].children[].value = OPC itemID）。
// 写命令 data JSON: {"item":"<itemID>","value":<值>}（值按 item 原生类型自动转换）。
//
// 仅 Windows（opcda 依赖 COM）；非 Windows 编译本文件被 build tag 排除。
package main

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"sync"
	"time"

	"github.com/huskar-t/opcda"
	"github.com/huskar-t/opcda/com"
)

// bridge 协议帧类型（与 rs-iot docs/external-plugin-protocol.md 一致）。
const (
	tHello      byte = 0x01
	tHelloAck   byte = 0x02
	tConfigPush byte = 0x03
	tDataUp     byte = 0x04
	tCommand    byte = 0x05
	tCommandAck byte = 0x06
	tHeartbeat  byte = 0x07
)

const protoVer = "1"

// ConfigPush 结构（bridge → ext）。
type configPush struct {
	InstID  string       `json:"inst_id"`
	Devices []deviceSpec `json:"devices"`
}

type deviceSpec struct {
	Label    string      `json:"label"`
	Value    string      `json:"value"` // 设备 ID（DataUp 的 devid）
	Children []childSpec `json:"children"`
}

type childSpec struct {
	Label string `json:"label"`
	Value string `json:"value"` // OPC itemID
	Type  string `json:"type"`
}

// DataUp（ext → bridge）。
type dataUp struct {
	Devid   string       `json:"devid"`
	Samples []dataSample `json:"samples"`
}

type dataSample struct {
	Tag   string      `json:"tag"`
	Value interface{} `json:"value"`
	Ts    int64       `json:"ts"`
}

// ===== 帧编解码 =====

// writeFrame 组帧 [4B BE len][1B type][JSON body] 并写入。
func writeFrame(w io.Writer, typ byte, body interface{}) error {
	payload := []byte{typ}
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return err
		}
		payload = append(payload, b...)
	}
	var lenBuf [4]byte
	binary.BigEndian.PutUint32(lenBuf[:], uint32(len(payload)))
	if _, err := w.Write(lenBuf[:]); err != nil {
		return err
	}
	_, err := w.Write(payload)
	return err
}

// readFrame 读一帧 → (type, body raw)。EOF/错误返回 err。
func readFrame(r io.Reader) (byte, json.RawMessage, error) {
	var lenBuf [4]byte
	if _, err := io.ReadFull(r, lenBuf[:]); err != nil {
		return 0, nil, err
	}
	length := binary.BigEndian.Uint32(lenBuf[:])
	if length == 0 {
		return 0, nil, fmt.Errorf("empty frame")
	}
	payload := make([]byte, length)
	if _, err := io.ReadFull(r, payload); err != nil {
		return 0, nil, err
	}
	return payload[0], payload[1:], nil
}

// ===== OPC 值转换 =====

// opcValueToJSON：opcda 读出的 Go 值 → JSON 可序列化（time.Time → RFC3339 字符串）。
func opcValueToJSON(v interface{}) interface{} {
	switch x := v.(type) {
	case bool, string, float32, float64:
		return x
	case time.Time:
		return x.Format(time.RFC3339Nano)
	case int, int8, int16, int32, int64, uint, uint8, uint16, uint32, uint64:
		return x
	default:
		return fmt.Sprint(v)
	}
}

// toFloat：JSON 数字（float64）或数值字符串 → float64。
func toFloat(v interface{}) (float64, bool) {
	switch x := v.(type) {
	case float64:
		return x, true
	case float32:
		return float64(x), true
	case int:
		return float64(x), true
	case int64:
		return float64(x), true
	case string:
		var f float64
		_, err := fmt.Sscanf(x, "%g", &f)
		return f, err == nil
	}
	return 0, false
}

// convertValue：JSON 值 → item 原生 Go 类型（按 com.VT），供 opcda Write。
// 避免 BadTypeMismatch（OPC DA 严格类型，如 int16 item 写 float64 会拒绝）。
func convertValue(jsonVal interface{}, vt com.VT) (interface{}, error) {
	switch vt {
	case com.VT_BOOL:
		if b, ok := jsonVal.(bool); ok {
			return b, nil
		}
		return nil, fmt.Errorf("VT_BOOL 需要 bool")
	case com.VT_I1:
		f, ok := toFloat(jsonVal)
		if !ok {
			return nil, fmt.Errorf("VT_I1 需要数字")
		}
		return int8(f), nil
	case com.VT_UI1:
		f, ok := toFloat(jsonVal)
		if !ok {
			return nil, fmt.Errorf("VT_UI1 需要数字")
		}
		return uint8(f), nil
	case com.VT_I2:
		f, ok := toFloat(jsonVal)
		if !ok {
			return nil, fmt.Errorf("VT_I2 需要数字")
		}
		return int16(f), nil
	case com.VT_UI2:
		f, ok := toFloat(jsonVal)
		if !ok {
			return nil, fmt.Errorf("VT_UI2 需要数字")
		}
		return uint16(f), nil
	case com.VT_I4:
		f, ok := toFloat(jsonVal)
		if !ok {
			return nil, fmt.Errorf("VT_I4 需要数字")
		}
		return int32(f), nil
	case com.VT_UI4:
		f, ok := toFloat(jsonVal)
		if !ok {
			return nil, fmt.Errorf("VT_UI4 需要数字")
		}
		return uint32(f), nil
	case com.VT_I8:
		f, ok := toFloat(jsonVal)
		if !ok {
			return nil, fmt.Errorf("VT_I8 需要数字")
		}
		return int64(f), nil
	case com.VT_UI8:
		f, ok := toFloat(jsonVal)
		if !ok {
			return nil, fmt.Errorf("VT_UI8 需要数字")
		}
		return uint64(f), nil
	case com.VT_R4:
		f, ok := toFloat(jsonVal)
		if !ok {
			return nil, fmt.Errorf("VT_R4 需要数字")
		}
		return float32(f), nil
	case com.VT_R8:
		f, ok := toFloat(jsonVal)
		if !ok {
			return nil, fmt.Errorf("VT_R8 需要数字")
		}
		return f, nil
	case com.VT_BSTR:
		return fmt.Sprint(jsonVal), nil
	case com.VT_DATE:
		switch x := jsonVal.(type) {
		case string:
			t, err := time.Parse(time.RFC3339, x)
			if err != nil {
				return nil, fmt.Errorf("VT_DATE 解析失败（需 RFC3339）: %v", err)
			}
			return t, nil
		case float64:
			return time.Unix(int64(x), 0), nil
		}
		return nil, fmt.Errorf("VT_DATE 需要 RFC3339 字符串或 Unix 秒")
	default:
		return nil, fmt.Errorf("不支持的 OPC 类型 VT=%d（VT_CY currency 等 opcda 不支持）", vt)
	}
}

// ===== 插件 =====

type plugin struct {
	instID     string
	token      string
	bridgeHost string
	bridgePort int
	progID     string
	opcHost    string
	period     int
	hbTimeout  int // 从 HelloAck 拿
}

func (p *plugin) run() {
	if err := com.Initialize(); err != nil {
		log.Fatalf("[%s] com.Initialize: %v", p.instID, err)
	}
	defer com.Uninitialize()

	backoff := time.Second
	for {
		if err := p.session(); err != nil {
			log.Printf("[%s] session 结束: %v；%v 后重连", p.instID, err, backoff)
		}
		time.Sleep(backoff)
		if backoff < 30*time.Second {
			backoff *= 2
		}
	}
}

// session：一次 bridge 连接的完整生命周期（连接→握手→配置→采集/命令/心跳→断开）。
func (p *plugin) session() error {
	conn, err := net.Dial("tcp", fmt.Sprintf("%s:%d", p.bridgeHost, p.bridgePort))
	if err != nil {
		return fmt.Errorf("dial bridge: %w", err)
	}
	defer conn.Close()

	// 互斥写（采集/命令ack/心跳并发写同一 conn）。
	var writeMu sync.Mutex
	write := func(typ byte, body interface{}) error {
		writeMu.Lock()
		defer writeMu.Unlock()
		return writeFrame(conn, typ, body)
	}

	// 1. Hello。
	if err := write(tHello, map[string]string{
		"inst_id": p.instID, "token": p.token, "ver": protoVer,
	}); err != nil {
		return fmt.Errorf("send hello: %w", err)
	}

	// 2. HelloAck。
	typ, body, err := readFrame(conn)
	if err != nil {
		return fmt.Errorf("read helloack: %w", err)
	}
	if typ != tHelloAck {
		return fmt.Errorf("期望 HelloAck，收到 type=%d", typ)
	}
	var ack struct {
		OK            bool   `json:"ok"`
		Msg           string `json:"msg"`
		HeartbeatSecs int    `json:"heartbeat_secs"`
	}
	if err := json.Unmarshal(body, &ack); err != nil {
		return fmt.Errorf("解析 HelloAck: %w", err)
	}
	if !ack.OK {
		return fmt.Errorf("握手被拒: %s", ack.Msg)
	}
	p.hbTimeout = ack.HeartbeatSecs
	log.Printf("[%s] 握手成功，心跳=%ds", p.instID, p.hbTimeout)

	// 3. ConfigPush。
	typ, body, err = readFrame(conn)
	if err != nil {
		return fmt.Errorf("read configpush: %w", err)
	}
	if typ != tConfigPush {
		return fmt.Errorf("期望 ConfigPush，收到 type=%d", typ)
	}
	var cfg configPush
	if err := json.Unmarshal(body, &cfg); err != nil {
		return fmt.Errorf("解析 ConfigPush: %w", err)
	}
	itemIDs, devid := collectItems(cfg)
	if len(itemIDs) == 0 {
		return fmt.Errorf("ConfigPush 无 items")
	}
	log.Printf("[%s] 配置：devid=%s，%d items", p.instID, devid, len(itemIDs))

	// 4. 连 DA Server + AddItems（每 session 重建：bridge 断 → opcda 断 → 重连同）。
	server, err := opcda.Connect(p.progID, p.opcHost)
	if err != nil {
		return fmt.Errorf("opcda.Connect(%s@%s): %w", p.progID, p.opcHost, err)
	}
	defer server.Disconnect()
	groups := server.GetOPCGroups()
	group, err := groups.Add(p.instID)
	if err != nil {
		return fmt.Errorf("Add group: %w", err)
	}
	defer group.Release()
	items := group.OPCItems()
	itemList, addErrs, err := items.AddItems(itemIDs)
	if err != nil {
		return fmt.Errorf("AddItems: %w", err)
	}
	// itemID → item map（跳过 AddItem 失败的，如 VT_CY Money 不支持）。
	itemMap := make(map[string]*opcda.OPCItem, len(itemList))
	handles := make([]uint32, 0, len(itemList))
	validIDs := make([]string, 0, len(itemList))
	for i, it := range itemList {
		if addErrs[i] != nil {
			log.Printf("[%s] AddItem %s 失败（跳过）: %v", p.instID, itemIDs[i], addErrs[i])
			continue
		}
		itemMap[itemIDs[i]] = it
		handles = append(handles, it.GetServerHandle())
		validIDs = append(validIDs, itemIDs[i])
	}
	if len(handles) == 0 {
		return fmt.Errorf("无有效 item（全部 AddItem 失败）")
	}
	log.Printf("[%s] %d/%d items 就绪", p.instID, len(handles), len(itemIDs))

	// 等 DA Server 就绪（opcda example 习惯）。
	time.Sleep(time.Second)

	// 5. 并发：读命令 / 采集 / 心跳。任一结束 → cancel 其他。
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	var wg sync.WaitGroup

	// 读 goroutine：持续读 Command（连接断 → err → cancel 全部）。
	wg.Add(1)
	go func() {
		defer wg.Done()
		defer cancel()
		for {
			t, b, err := readFrame(conn)
			if err != nil {
				if err != io.EOF {
					log.Printf("[%s] 读结束: %v", p.instID, err)
				}
				return
			}
			if t == tCommand {
				handleCommand(b, itemMap, write)
			}
			// bridge 实际不发 Heartbeat/ConfigPush（仅握手期）；这里忽略其他帧。
		}
	}()

	// 采集 ticker。
	wg.Add(1)
	go func() {
		defer wg.Done()
		ticker := time.NewTicker(time.Duration(p.period) * time.Second)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				collectOnce(p.instID, group, handles, validIDs, devid, write)
			}
		}
	}()

	// 心跳 ticker。
	wg.Add(1)
	go func() {
		defer wg.Done()
		hb := p.hbTimeout
		if hb <= 0 {
			hb = 15
		}
		interval := time.Duration(hb/2) * time.Second
		if interval < time.Second {
			interval = time.Second
		}
		ticker := time.NewTicker(interval)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				if err := write(tHeartbeat, map[string]int64{"ts": time.Now().Unix()}); err != nil {
					log.Printf("[%s] 心跳发送失败: %v", p.instID, err)
					cancel()
					return
				}
			}
		}
	}()

	wg.Wait()
	return ctx.Err()
}

// collectItems：从 ConfigPush 收集所有 itemID + 第一个 devid。
func collectItems(cfg configPush) (ids []string, devid string) {
	for _, dev := range cfg.Devices {
		if devid == "" {
			devid = dev.Value
		}
		for _, ch := range dev.Children {
			ids = append(ids, ch.Value)
		}
	}
	return ids, devid
}

// collectOnce：一次 SyncRead → DataUp。
func collectOnce(instID string, group *opcda.OPCGroup, handles []uint32, ids []string, devid string, write func(byte, interface{}) error) {
	status, errs, err := group.SyncRead(opcda.OPC_DS_CACHE, handles)
	if err != nil {
		log.Printf("[%s] SyncRead 失败: %v", instID, err)
		return
	}
	samples := make([]dataSample, 0, len(status))
	ts := time.Now().Unix()
	for i, st := range status {
		if errs[i] != nil {
			log.Printf("[%s] 读 %s 异常: %v", instID, ids[i], errs[i])
			continue
		}
		samples = append(samples, dataSample{
			Tag:   ids[i],
			Value: opcValueToJSON(st.Value),
			Ts:    ts,
		})
	}
	if len(samples) == 0 {
		return
	}
	if err := write(tDataUp, dataUp{Devid: devid, Samples: samples}); err != nil {
		log.Printf("[%s] DataUp 发送失败: %v", instID, err)
	}
}

// handleCommand：写命令 → opcda 写 item → CommandAck。
func handleCommand(body json.RawMessage, itemMap map[string]*opcda.OPCItem, write func(byte, interface{}) error) {
	var cmd struct {
		CmdID uint64 `json:"cmd_id"`
		Devid string `json:"devid"`
		Cmd   string `json:"cmd"`
		Data  string `json:"data"`
	}
	if err := json.Unmarshal(body, &cmd); err != nil {
		log.Printf("解析 Command 失败: %v", err)
		return
	}
	ack := map[string]interface{}{"cmd_id": cmd.CmdID}
	// data JSON: {"item":"<itemID>","value":<值>}。
	var payload struct {
		Item  string      `json:"item"`
		Value interface{} `json:"value"`
	}
	if err := json.Unmarshal([]byte(cmd.Data), &payload); err != nil {
		ack["ok"] = false
		ack["err"] = fmt.Sprintf("解析 data 失败: %v", err)
		_ = write(tCommandAck, ack)
		return
	}
	item, ok := itemMap[payload.Item]
	if !ok {
		ack["ok"] = false
		ack["err"] = fmt.Sprintf("item %s 不在配置中", payload.Item)
		_ = write(tCommandAck, ack)
		return
	}
	goVal, err := convertValue(payload.Value, item.GetCanonicalDataType())
	if err != nil {
		ack["ok"] = false
		ack["err"] = err.Error()
		_ = write(tCommandAck, ack)
		return
	}
	if err := item.Write(goVal); err != nil {
		ack["ok"] = false
		ack["err"] = err.Error()
	} else {
		ack["ok"] = true
		log.Printf("写 %s = %v（VT=%d）成功", payload.Item, goVal, item.GetCanonicalDataType())
	}
	_ = write(tCommandAck, ack)
}

func main() {
	p := &plugin{}
	flag.StringVar(&p.instID, "inst", "", "实例 ID（rs-iot 注册的 inst_id）")
	flag.StringVar(&p.token, "token", "", "plugin token（建实例时 rs-iot 返回一次）")
	flag.StringVar(&p.bridgeHost, "bridge-host", "127.0.0.1", "rs-iot bridge 地址")
	flag.IntVar(&p.bridgePort, "bridge-port", 8781, "rs-iot bridge 端口")
	flag.StringVar(&p.progID, "prog-id", "Matrikon.OPC.Simulation.1", "OPC DA Server ProgID")
	flag.StringVar(&p.opcHost, "opc-host", "localhost", "OPC DA Server 主机")
	flag.IntVar(&p.period, "period", 2, "采集周期（秒）")
	flag.Parse()
	if p.instID == "" || p.token == "" {
		log.Fatal("--inst 和 --token 必填")
	}
	log.Printf("[%s] 启动：bridge %s:%d，OPC %s@%s，period=%ds",
		p.instID, p.bridgeHost, p.bridgePort, p.progID, p.opcHost, p.period)
	p.run()
}
