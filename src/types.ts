export type Drive = {
  id: string;
  name: string;
  size_bytes: number;
  removable: boolean;
  device_path: string;
};

export type FlashProgress = {
  bytes_written: number;
  total_bytes: number;
  bytes_per_second: number;
  phase: "flashing" | "verifying" | "done";
};
