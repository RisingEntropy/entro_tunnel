// Icon set — thin wrappers over lucide-react (https://lucide.dev, ISC-licensed),
// so the geometry is professionally drawn instead of hand-rolled. Every export
// keeps the original `IconX` name + `{ size?, className? }` API, so App.tsx is
// untouched. Default size 20; stroke scales with size (no absoluteStrokeWidth)
// to match the previous look.
import {
  ArrowDown,
  ArrowUp,
  Download,
  Globe,
  Layers,
  LayoutDashboard,
  Moon,
  Network,
  Pencil,
  Plus,
  Power,
  ScrollText,
  Server,
  Settings,
  ShieldCheck,
  Split,
  Sun,
  Trash2,
  Upload,
  Wifi,
  X,
  type LucideIcon,
  type LucideProps,
} from "lucide-react";

export type IconProps = LucideProps & { size?: number };

const make = (Icon: LucideIcon) =>
  function Wrapped({ size = 20, strokeWidth = 1.9, ...props }: IconProps) {
    return <Icon size={size} strokeWidth={strokeWidth} {...props} />;
  };

export const IconDashboard = make(LayoutDashboard);
export const IconProxy = make(Network);
export const IconProfiles = make(Layers);
export const IconConnections = make(Globe);
export const IconRules = make(Split);
export const IconLogs = make(ScrollText);
export const IconSettings = make(Settings);
export const IconPower = make(Power);
export const IconUpload = make(ArrowUp);
export const IconDownload = make(ArrowDown);
export const IconServer = make(Server);
export const IconWifi = make(Wifi);
export const IconPlus = make(Plus);
export const IconTrash = make(Trash2);
export const IconEdit = make(Pencil);
export const IconImport = make(Download);
export const IconExport = make(Upload);
export const IconArrowUp = make(ArrowUp);
export const IconArrowDown = make(ArrowDown);
export const IconClose = make(X);
export const IconShield = make(ShieldCheck);
export const IconSun = make(Sun);
export const IconMoon = make(Moon);
