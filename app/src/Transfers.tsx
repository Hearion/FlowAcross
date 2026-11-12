// 侧栏的「文件」面板：等待手动接收的文件、正在进行和最近结束的跨设备复制。
// 没有内容时整个面板不显示。

import { api } from "./api";
import type { FileOfferView, StateView, TransferView } from "./types";
import { CloseIcon, FolderIcon, TransferIcon } from "./ui";
import { ProgressBar } from "./Updater";

/** 与 flow-core 的 files::AUTO_FETCH_LIMIT 一致：超过它的文件不自动接收。 */
const AUTO_FETCH_LIMIT = 1024 * 1024 * 1024;

/** 1.2 KB / 34.5 MB / 3.2 GB。 */
export function formatBytes(n: number): string {
  if (n < 1000) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n;
  let i = -1;
  do {
    v /= 1000;
    i++;
  } while (v >= 1000 && i < units.length - 1);
  return `${v >= 100 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

function statusText(t: TransferView): string {
  const progress = `${formatBytes(t.done)} / ${formatBytes(t.total)}`;
  switch (t.status) {
    case "running":
      return t.incoming ? `正在从${t.peer_name}接收，${progress}` : `正在发给${t.peer_name}，${progress}`;
    case "done":
      if (!t.incoming) return `已发给${t.peer_name}`;
      return t.pasteable ? "已收到，可以直接粘贴" : "已收到，保存在下载文件夹的「流界」里";
    case "canceled":
      return "已取消";
    case "failed":
      return t.error ?? "传输失败";
  }
}

function OfferCard({ offer }: { offer: FileOfferView }) {
  return (
    <div className="file-offer">
      <div className="file-offer-text">
        <span>
          {offer.peer_name}复制了「{offer.label}」
        </span>
        <small>
          {offer.files} 个文件，{formatBytes(offer.bytes)}。
          {offer.bytes > AUTO_FETCH_LIMIT ? "较大，不会自动传输" : "还没有接收"}
        </small>
      </div>
      <button className="btn small primary" onClick={() => api.acceptFiles()}>
        接收
      </button>
    </div>
  );
}

function TransferRow({ t }: { t: TransferView }) {
  const running = t.status === "running";
  const pct = t.total > 0 ? Math.min(1, t.done / t.total) : 1;
  return (
    <li className={`transfer ${t.status}`}>
      <span className="transfer-icon">
        <TransferIcon incoming={t.incoming} />
      </span>
      <span className="transfer-info">
        <span className="transfer-name" title={t.label}>
          {t.label}
        </span>
        <span className="transfer-sub">{statusText(t)}</span>
        {running && (
          <span
            className="transfer-progress"
            role="progressbar"
            aria-label={`「${t.label}」的传输进度`}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={Math.round(pct * 100)}
          >
            <ProgressBar value={pct} />
          </span>
        )}
      </span>
      {running ? (
        <button className="btn small" onClick={() => api.cancelTransfer(t.id)}>
          取消
        </button>
      ) : (
        <button
          className="btn icon small ghost"
          title="从列表中移除"
          aria-label={`从列表中移除「${t.label}」`}
          onClick={() => api.dismissTransfer(t.id)}
        >
          <CloseIcon />
        </button>
      )}
    </li>
  );
}

export function FileTransfers({ state, onError }: { state: StateView; onError: (msg: string) => void }) {
  if (state.transfers.length === 0 && !state.file_offer) return null;
  return (
    <section className="panel">
      <div className="panel-head">
        <h2>文件</h2>
        {state.receive_dir && (
          <button className="btn small ghost-link" onClick={() => api.openReceivedFiles().catch((e) => onError(String(e)))}>
            <FolderIcon />
            打开文件夹
          </button>
        )}
      </div>
      {state.file_offer && <OfferCard offer={state.file_offer} />}
      {state.transfers.length > 0 && (
        <ul className="transfer-list">
          {state.transfers.map((t) => (
            <TransferRow key={t.id} t={t} />
          ))}
        </ul>
      )}
    </section>
  );
}
