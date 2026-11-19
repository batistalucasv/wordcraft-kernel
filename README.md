# WordCraft Kernel

Motor de execução segura e isolada de código (**R** e **Python**) para o ecossistema do **WordCraft** e computação científica interativa.

Desenvolvido especialmente para pesquisa em **Ecologia**, ciência de dados e **ensino prático de programação e estatística**, permitindo a criação de blocos de código (*code chunks*) no estilo Quarto / R Markdown / Jupyter dentro de processadores de texto nativos.

---

## Características

- **100% Rust Nativo:** Sem C-FFI instável, sem código inseguro (`unsafe_code = "forbid"`), sem panics (`clippy::unwrap_used` negado no código de produção).
- **Paridade R e Python:** Mesma interface, mesma captura de saídas e tratamento de erros.
- **Proteção "Never Crash":** Execução em processos isolados com controle de tempo (*timeout* ajustável) para encerrar scripts travados ou em loop infinito.
- **Captura Automática de Gráficos:** Detecta e extrai plots em alta resolução gerados por:
  - **R:** Funções básicas (`plot()`, `hist()`, `boxplot()`) e pacotes modernos (`ggplot2`).
  - **Python:** `matplotlib`, `seaborn`, etc.
- **Captura Limpa de Saída:** Separação estrita de `stdout`, `stderr`, código de saída e tempo de execução decorrido.
- **Biblioteca de Templates:** Trechos prontos e comentados de ecologia e estatística em R e Python, descritos em `templates`.

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

## Biblioteca de templates

O módulo `wordcraft_kernel::templates` traz sete trechos completos e comentados
que já rodam com dados sintéticos:

| ID do template | Linguagem | Categoria | Pacotes |
|---|---|---|---|
| `r.shannon-simpson` | R | Diversidade | `vegan` |
| `r.species-accumulation` | R | Acumulação de espécies | `vegan` |
| `r.lm-confidence` | R | Regressão | `ggplot2` |
| `r.ttest-anova` | R | Testes de hipótese | `ggplot2` |
| `python.data-ingest` | Python | Ingestão de dados | `pandas` |
| `python.correlation-heatmap` | Python | Correlação | `pandas`, `seaborn`, `matplotlib`, `scipy` |
| `python.lm-tests` | Python | Regressão | `numpy`, `pandas`, `scipy` |

Navegue com `templates::all()`, `templates::find(id)`,
`templates::by_language(...)` e `templates::by_category(...)`, e então renderize
e execute um deles:

```rust
use wordcraft_kernel::{execute, templates, ExecutionRequest, KernelError};

fn main() -> Result<(), KernelError> {
    let template = templates::find("r.ttest-anova").expect("template embutido");
    let script = template.render(&[("title", "Biomassa em três áreas")])?;
    let result = execute(&ExecutionRequest::new(template.language, script))?;
    println!("{}", result.stdout);
    Ok(())
}
```

Todo parâmetro tem um valor padrão, então `render_default()` sempre produz um
script executável. Os valores são inseridos literalmente, porque são código
(uma expressão R, um nome de coluna, um caminho); cite ou valide entradas não
confiáveis para a linguagem de destino antes de renderizar. A suíte de testes
executa cada trecho quando o interpretador e os pacotes listados estão
instalados, e o ignora com uma mensagem caso contrário.

---

## Testes

Para executar os testes automatizados:

```bash
cargo test
```

---

## Licença

Distribuído sob as licenças **MIT** ou **Apache-2.0**.
