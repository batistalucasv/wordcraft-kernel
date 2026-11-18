# WordCraft Kernel

Motor de execução segura e isolada de código (**R** e **Python**) para o ecossistema do **WordCraft** e computação científica interativa.

Desenvolvido especialmente para pesquisa em **Ecologia**, ciência de dados e **ensino prático de programação e estatística**, permitindo a criação de blocos de código (*code chunks*) no estilo Quarto / R Markdown / Jupyter dentro de processadores de texto nativos.

---

## Características

- **100% Rust Nativo:** Sem C-FFI instável, sem código inseguro (`unsafe_code = "forbid"`), sem panics (`clippy::unwrap_used` negado no código de produção).
- **Paridade R e Python:** Mesma interface, mesma captura de saídas e tratamento de erros.
- **Proteção "Never Crash":** Execução em processos isolados com controle de tempo (*timeout* ajustável) para encerrar scripts travados ou em loop infinito.
- **Captura Automática de Gráficos:** Detecta e extrai plots tanto em formato vetorial sem perda (**SVG**) quanto raster em alta resolução (**PNG**) gerados por:
  - **R:** Funções básicas (`plot()`, `hist()`, `boxplot()`) e pacotes modernos (`ggplot2`).
  - **Python:** `matplotlib`, `seaborn`, etc.
  - Suporte aos modos: `PlotFormat::Png`, `PlotFormat::Svg` e `PlotFormat::Both`.
- **Captura Limpa de Saída:** Separação estrita de `stdout`, `stderr`, código de saída e tempo de execução decorrido.

---

## Como Usar

### Adicionando como Dependência

No seu `Cargo.toml`:

```toml
[dependencies]
wordcraft-kernel = { git = "https://github.com/batistalucasv/wordcraft-kernel" }
```

### Exemplo em Rust

```rust
use wordcraft_kernel::{execute, CodeLanguage, ExecutionRequest};

fn main() {
    // Exemplo com R
    let r_code = r#"
        especies <- c("Sp1", "Sp2", "Sp3")
        abundancia <- c(10, 25, 5)
        cat("Total de indivíduos:", sum(abundancia), "\n")
        barplot(abundancia, names.arg = especies, main = "Abundância")
    "#;

    let req = ExecutionRequest::new(CodeLanguage::R, r_code)
        .with_timeout(10_000)
        .with_plot_capture(true);

    match execute(&req) {
        Ok(result) => {
            println!("Saída do R:\n{}", result.stdout);
            if let Some(png_bytes) = result.plot_png {
                println!("Gráfico gerado! Tamanho: {} bytes", png_bytes.len());
            }
        }
        Err(err) => eprintln!("Erro ao executar: {err}"),
    }

    // Exemplo com Python
    let py_code = r#"
        import numpy as np
        dados = np.array([12, 15, 18, 22, 30])
        print("Média:", np.mean(dados))
    "#;

    let req_py = ExecutionRequest::new(CodeLanguage::Python, py_code);
    if let Ok(res) = execute(&req_py) {
        println!("Saída do Python:\n{}", res.stdout);
    }
}
```

---

## Testes

Para executar os testes automatizados:

```bash
cargo test
```

---

## Licença

Distribuído sob as licenças **MIT** ou **Apache-2.0**.
