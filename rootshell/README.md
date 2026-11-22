# Сопряжение через rootshell

`siboot-pair` — дополнительный способ сопрячь iPhone с самим собой, если в Open Minis нет встроенного сопряжения. Нужны iOS 27 или новее, включённый режим разработчика и rootshell с разрешённым доступом к локальной сети.

Исходный проект SideInstaller: [FrizzleM/SideInstaller](https://github.com/FrizzleM/SideInstaller). Русская версия siboot и её поддержка — ripcats.

В rootshell включите **Settings → Location Diary Mode → Session Only**, затем выполните команды по одной:

```sh
curl -fsSLO https://raw.githubusercontent.com/ripcats/siboot-ru/main/rootshell/bin/siboot-pair.wasm
ifconfig en0 > ifconfig.txt
./siboot-pair.wasm
```

В настройках iOS откройте **Конфиденциальность и безопасность → Режим разработчика → Pair with siboot** и введите код. Не закрывая rootshell, запустите siboot в терминале Open Minis:

```sh
wget -qO- https://raw.githubusercontent.com/ripcats/siboot-ru/main/minis/install.sh | sh
```

При первом запуске siboot запросит код передачи, который показывает `siboot-pair`.

Для доступа к серверам Apple при входе и установке может понадобиться обычный VPN. Без него возможны тайм-ауты и ошибки. siboot не может проверить или переключить VPN; следуйте его подсказкам и переключайте соединение вручную.

Сборку WASM-помощника выполняет GitHub Actions. Локальную компиляцию без отдельного разрешения не запускайте.

Проект поддерживает [ripcats](https://github.com/ripcats).
