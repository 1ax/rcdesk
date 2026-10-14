Базовые растры значка трея/строки меню (сырой RGBA, straight alpha, row-major):
`mono-44.rgba` -- 44x44, чёрный + альфа (macOS template),
`color-32.rgba` -- 32x32, цветной логотип (Windows).
Источник -- `installer/logo.svg`; файлы генерирует `python3 installer/make-icons.py`
(из корня репозитория, нужен macOS: qlmanage/sips/iconutil). Руками не править.
