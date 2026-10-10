# Toolbox for generating test fixtures (not part of the service image).
FROM python:3.12-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
      libreoffice-writer-nogui libreoffice-calc-nogui libreoffice-impress-nogui \
      fonts-noto-core fonts-dejavu-core qpdf libraqm0 libfribidi0 libharfbuzz0b \
    && rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir \
      python-docx==1.1.2 openpyxl==3.1.5 python-pptx==1.0.2 reportlab==4.2.5 \
      pillow==11.0.0 msoffcrypto-tool==5.4.2 arabic-reshaper==3.0.0 python-bidi==0.6.3 fonttools==4.55.0
WORKDIR /work
